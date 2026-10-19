/* ═════════════════════════════════════════════════════════════════════
   design/settings/rows-remote.jsx — custom Settings rows of Remote access
   (window.OMP_SETTINGS_ROWS). Rust owns `remote.json`; every row shows
   `remoteStatus()` (read on mount, refreshed on `remote://status`) and
   saves by sending the COMPLETE settings object to `remoteApply`.
     remoteServer   enable, interface, port, host name, plain HTTP, status
     remoteCert     generated or own files; fingerprint; regenerate
     remotePair     one-time QR code dialog
     remoteDevices  paired phones: rename, revoke (asks first)
     remotePush     Web Push: triggers, VAPID contact, test notification
   The QR is an <img> of the SVG as a data URL — never injected as HTML.
   ═════════════════════════════════════════════════════════════════════ */

const { Icon: _SRR_Icon } = window;

// Settings are sent whole, so overlapping applies must not start from a stale
// copy: every row shares the newest known status/settings and mutations run
// one by one through a single shared listener.
let _SRR_latest = null;
let _SRR_status = null;
let _SRR_queue = Promise.resolve();
const _SRR_subs = new Set();
let _SRR_bridge = null;
let _SRR_stop = null;
let _SRR_connecting = null;

function _SRR_publish(value, updateSettings = true) {
  if (!value) return;
  if (updateSettings) _SRR_latest = value.settings ?? _SRR_latest;
  else _SRR_latest ??= value.settings ?? null;
  _SRR_status = value;
  for (const sub of _SRR_subs) sub(value);
}

function _SRR_retain(bridge, setStatus) {
  if (_SRR_bridge !== bridge) {
    _SRR_stop?.();
    _SRR_stop = null;
    _SRR_connecting = null;
    _SRR_bridge = bridge;
    _SRR_latest = null;
    _SRR_status = null;
  }
  _SRR_subs.add(setStatus);
  if (_SRR_status) setStatus(_SRR_status);
  if (!_SRR_stop && !_SRR_connecting && bridge) {
    let statusSeen = false;
    const myBridge = bridge;
    _SRR_connecting = Promise.resolve(bridge.onRemoteEvent("status", value => {
      if (_SRR_bridge === myBridge && value) {
        statusSeen = true;
        _SRR_publish(value, true);
      }
    })).then(unlisten => {
      _SRR_connecting = null;
      if (_SRR_bridge === myBridge && _SRR_subs.size > 0) _SRR_stop = unlisten;
      else unlisten?.();
    }).then(() => _SRR_bridge === myBridge ? bridge.remoteStatus() : null).then(value => {
      if (_SRR_bridge === myBridge && !statusSeen && value) _SRR_publish(value, false);
    });
  }
  return () => {
    _SRR_subs.delete(setStatus);
    if (_SRR_subs.size === 0 && _SRR_stop) {
      _SRR_stop();
      _SRR_stop = null;
    }
  };
}

/** Status + interfaces for one row; `apply(patch)` merges into the newest saved
 *  settings inside the queue and sends them whole. `patch.push` is partial: its
 *  fields merge into the newest `push`, so delayed toggles never overwrite
 *  each other. */
function _SRR_useRemote(bridge) {
  const [status, setStatus] = React.useState(_SRR_bridge === bridge ? _SRR_status : null);
  const [error, setError] = React.useState(null);
  React.useEffect(() => _SRR_retain(bridge, setStatus), [bridge]);
  const runNow = React.useCallback(async (call, updateSettings = false) => {
    const result = await call();
    if (result?.ok === false) { setError(result.error ?? "failed"); return false; }
    setError(null);
    if (result?.value) _SRR_publish(result.value, updateSettings);
    return true;
  }, []);
  const run = React.useCallback((call) => {
    const next = _SRR_queue.then(() => runNow(call, false));
    _SRR_queue = next.catch(() => {});
    return next;
  }, [runNow]);
  const apply = React.useCallback((patch) => {
    if (!_SRR_latest && !status?.settings) return Promise.resolve(false);
    const next = _SRR_queue.then(() => {
      const base = _SRR_latest ?? status.settings;
      const patchValue = typeof patch === "function" ? patch(base) : patch;
      const merged = { ...base, ...patchValue };
      if (patchValue.push) merged.push = { ...base.push, ...patchValue.push };
      return runNow(() => bridge.remoteApply(merged), true);
    });
    _SRR_queue = next.catch(() => {});
    return next;
  }, [bridge, status, runNow]);
  return { status, error, apply, run, setError };
}

function _SRR_useAsync(load, fallback, deps) {
  const [value, setValue] = React.useState(fallback);
  React.useEffect(() => {
    let live = true;
    Promise.resolve(load()).then(v => { if (live && v) setValue(v); });
    return () => { live = false; };
  }, deps);
  return value;
}

/** Text field committed on blur or Enter; re-syncs when the saved value changes. */
function _SRR_Field({ value, onCommit, label, placeholder, width, disabled, mono = true }) {
  const [text, setText] = React.useState(value);
  React.useEffect(() => setText(value), [value]);
  const commit = () => { if (text !== value) onCommit(text); };
  return (
    <input className="st-rem-input" type="text" value={text} aria-label={label} placeholder={placeholder}
      disabled={disabled} spellCheck={false} style={{ width, fontFamily: mono ? undefined : "inherit" }}
      onChange={e => setText(e.target.value)} onBlur={commit}
      onKeyDown={e => {
        if (e.key !== "Enter" || e.nativeEvent?.isComposing || e.keyCode === 229) return;
        e.currentTarget.blur();
      }} />
  );
}

function _SRR_Line({ label, children }) {
  return <div className="st-rem-line"><span className="st-rem-lbl">{label}</span>{children}</div>;
}

function _SRR_statusText(status) {
  if (!status) return "reading status…";
  if (status.error) return status.error;
  if (status.listening) return `listening on ${status.url ?? status.ip ?? ""}`;
  return status.enabled ? "starting…" : "off";
}

function _SRR_Server({ row, ctx, query }) {
  const Row = window.SettingsRow, Toggle = window.SettingsToggle;
  const { status, error, apply, run } = _SRR_useRemote(ctx.bridge);
  const ifaces = _SRR_useAsync(() => ctx.bridge.remoteInterfaces(), [], [ctx.bridge]);
  const s = status?.settings;
  const known = ifaces.some(i => i.name === s?.interface);
  return (
    <Row label={row.label} desc={row.desc} query={query} stack>
      <div className="st-rem">
        <_SRR_Line label="Enable">
          <Toggle value={!!s?.enabled} label="Enable remote access" onChange={v => apply({ enabled: v })} />
        </_SRR_Line>
        <_SRR_Line label="Interface">
          <select className="st-rem-input" value={s?.interface ?? ""} disabled={!s} aria-label="Network interface"
            onChange={e => apply({ interface: e.target.value || null })}>
            <option value="">choose…</option>
            {s?.interface && !known && <option value={s.interface}>{s.interface} (not found)</option>}
            {ifaces.map(i => <option key={i.name} value={i.name}>{i.name} · {i.ip}</option>)}
          </select>
        </_SRR_Line>
        <_SRR_Line label="Port">
          <_SRR_Field value={String(s?.port ?? "")} label="Port" width={90} disabled={!s}
            onCommit={text => { const n = Number(text); if (Number.isInteger(n) && n > 0 && n < 65536) apply({ port: n }); }} />
        </_SRR_Line>
        <_SRR_Line label="Host name">
          <_SRR_Field value={s?.hostName ?? ""} label="Host name" placeholder="e.g. omp.home.lan" width={240} disabled={!s}
            onCommit={text => apply({ hostName: text.trim() || null })} />
        </_SRR_Line>
        <_SRR_Line label="Allow plain HTTP">
          <Toggle value={!!s?.allowHttp} label="Allow plain HTTP" onChange={v => apply({ allowHttp: v })} />
        </_SRR_Line>
        {s?.allowHttp && (
          <div className="st-rem-warn">
            Plain HTTP sends the pairing code and token unencrypted and the phone cannot receive push
            notifications. Use it only on a network you trust.
          </div>
        )}
        <div className={`st-rem-status${status?.error ? " err" : ""}`}>{_SRR_statusText(status)}</div>
        {error && <div className="st-row-error">{error}</div>}
      </div>
    </Row>
  );
}

function _SRR_Cert({ row, ctx, query }) {
  const Row = window.SettingsRow, Btn = window.SettingsButton, Seg = window.SettingsSeg;
  const { status, error, apply, run } = _SRR_useRemote(ctx.bridge);
  const s = status?.settings;
  const saved = s?.cert?.kind === "files";
  // "own files" is only a draft until the paths are saved with the button.
  const [draftOwn, setDraftOwn] = React.useState(false);
  const own = saved || draftOwn;
  const [files, setFiles] = React.useState({ cert: "", key: "" });
  React.useEffect(() => { if (saved) setFiles({ cert: s.cert.cert, key: s.cert.key }); }, [saved, s?.cert?.cert, s?.cert?.key]);
  const saveFiles = () => apply({ cert: { kind: "files", cert: files.cert.trim(), key: files.key.trim() } });
  return (
    <Row label={row.label} desc={row.desc} query={query} stack>
      <div className="st-rem">
        <_SRR_Line label="Source">
          <Seg label="Certificate source" value={own ? "files" : "generated"}
            options={[{ value: "generated", label: "generated" }, { value: "files", label: "own files" }]}
            onChange={v => { if (v === "generated") { setDraftOwn(false); apply({ cert: { kind: "generated" } }); } else setDraftOwn(true); }} />
        </_SRR_Line>
        {own && (
          <>
            <_SRR_Line label="Certificate (PEM)">
              <input className="st-rem-input" type="text" value={files.cert} aria-label="Certificate file path" spellCheck={false}
                style={{ width: 320 }} onChange={e => setFiles(f => ({ ...f, cert: e.target.value }))} />
            </_SRR_Line>
            <_SRR_Line label="Private key (PEM)">
              <input className="st-rem-input" type="text" value={files.key} aria-label="Private key file path" spellCheck={false}
                style={{ width: 320 }} onChange={e => setFiles(f => ({ ...f, key: e.target.value }))} />
            </_SRR_Line>
            <div className="st-btns"><Btn onClick={saveFiles} disabled={!files.cert.trim() || !files.key.trim()}>Use these files</Btn></div>
          </>
        )}
        {!own && (
          <div className="st-btns">
            <Btn onClick={() => run(() => ctx.bridge.remoteRegenerateCert())} disabled={!s}>Regenerate</Btn>
          </div>
        )}
        {status?.fingerprint && <_SRR_Line label="SHA-256"><span className="st-path">{status.fingerprint}</span></_SRR_Line>}
        {error && <div className="st-row-error">{error}</div>}
      </div>
    </Row>
  );
}

/** Seconds left until `expiresAt` (unix seconds), never negative. */
const _SRR_left = (expiresAt, now) => Math.max(0, Math.ceil(expiresAt - now / 1000));

function _SRR_PairDialog({ pair, active = true, onClose }) {
  const [now, setNow] = React.useState(Date.now());
  React.useEffect(() => { const t = setInterval(() => setNow(Date.now()), 1000); return () => clearInterval(t); }, []);
  React.useEffect(() => {
    if (!active) return undefined;
    const onKey = e => { if (e.key === "Escape") { e.stopPropagation(); onClose(); } };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [active, onClose]);
  const left = _SRR_left(pair.expiresAt, now);
  const src = `data:image/svg+xml;base64,${btoa(unescape(encodeURIComponent(pair.svg)))}`;
  return (
    <div className="bridge-scrim" onClick={onClose} style={{ paddingTop: "12vh" }}>
      <div className="st-rem-dialog" role="dialog" aria-label="Pair a phone" data-st-esc="" onClick={e => e.stopPropagation()}>
        <div className="st-rem-dialog-head">
          <span>Pair a phone</span>
          <button type="button" className="st-ghost" onClick={onClose} title="close"><_SRR_Icon name="close" size={11} /></button>
        </div>
        {left > 0 ? (
          <>
            <img className="st-rem-qr" src={src} alt="Pairing QR code" width={240} height={240} />
            <div className="st-path">{pair.url}</div>
            <div className="st-rem-status">Scan with the phone camera · valid {left}s · one use</div>
          </>
        ) : <div className="st-rem-status err">The code expired. Close this dialog and pair again.</div>}
      </div>
    </div>
  );
}

function _SRR_Pair({ row, ctx, query }) {
  const Row = window.SettingsRow, Btn = window.SettingsButton;
  const { status } = _SRR_useRemote(ctx.bridge);
  const [pair, setPair] = React.useState(null);
  const [error, setError] = React.useState(null);
  const [busy, setBusy] = React.useState(false);
  const base = React.useRef(0);
  const count = status?.devices?.length ?? 0;
  const close = React.useCallback(() => { setPair(null); void ctx.bridge.remotePairCancel(); }, [ctx.bridge]);
  const liveRef = React.useRef(true);
  const pairing = React.useRef(false);
  React.useEffect(() => {
    liveRef.current = true;
    return () => {
      liveRef.current = false;
      if (pairing.current) { pairing.current = false; void ctx.bridge.remotePairCancel(); }
    };
  }, [ctx.bridge]);
  // A new device while the dialog is up means the code was used.
  React.useEffect(() => { if (pair && count > base.current) { pairing.current = false; setPair(null); } }, [count, pair]);
  const start = async () => {
    if (pairing.current) return;
    pairing.current = true;
    setBusy(true);
    try {
      const r = await ctx.bridge.remotePairStart();
      if (!liveRef.current) { if (r?.ok !== false) void ctx.bridge.remotePairCancel(); return; }
      if (r?.ok === false) { pairing.current = false; setError(r.error ?? "failed"); return; }
      setError(null);
      base.current = count;
      setPair(r.value);
    } finally {
      if (liveRef.current) setBusy(false);
    }
  };
  return (
    <Row label={row.label} desc={row.desc} query={query}>
      <Btn accent onClick={start} disabled={!status?.listening || busy || !!pair}
        title={status?.listening ? undefined : "Enable remote access first"}>Pair a phone…</Btn>
      {pair && <_SRR_PairDialog pair={pair} active={ctx.active !== false} onClose={() => { pairing.current = false; close(); }} />}
      {error && <div className="st-row-error">{error}</div>}
    </Row>
  );
}

function _SRR_Device({ device, onRename, onRevoke }) {
  const Btn = window.SettingsButton;
  const [asking, setAsking] = React.useState(false);
  return (
    <div className="st-rem-dev">
      <_SRR_Field value={device.name} label="Device name" mono={false} width={220} onCommit={text => text.trim() && onRename(device.id, text.trim())} />
      <span className="st-rem-sub">{device.connected ? "connected" : "offline"}{device.pushSubscribed ? " · push" : ""}</span>
      <div className="st-btns" style={{ marginLeft: "auto" }}>
        {asking ? (
          <>
            <Btn accent onClick={() => { setAsking(false); onRevoke(device.id); }}>Confirm revoke</Btn>
            <Btn onClick={() => setAsking(false)}>Keep</Btn>
          </>
        ) : <Btn onClick={() => setAsking(true)}>Revoke</Btn>}
      </div>
    </div>
  );
}

function _SRR_Devices({ row, ctx, query }) {
  const Row = window.SettingsRow;
  const { status, error, run } = _SRR_useRemote(ctx.bridge);
  const devices = status?.devices ?? [];
  return (
    <Row label={row.label} desc={row.desc} query={query} stack>
      <div className="st-rem">
        {devices.length === 0 && <div className="st-rem-status">No phone paired yet.</div>}
        {devices.map(d => (
          <_SRR_Device key={d.id} device={d}
            onRename={(id, name) => run(() => ctx.bridge.remoteDeviceRename(id, name))}
            onRevoke={id => run(() => ctx.bridge.remoteDeviceRevoke(id))} />
        ))}
        {error && <div className="st-row-error">{error}</div>}
      </div>
    </Row>
  );
}

function _SRR_Push({ row, ctx, query }) {
  const Row = window.SettingsRow, Toggle = window.SettingsToggle, Btn = window.SettingsButton;
  const { status, error, apply, run } = _SRR_useRemote(ctx.bridge);
  const push = status?.settings?.push;
  // Prefill an empty contact once from git; the user still has to save it.
  const suggest = _SRR_useAsync(() => ctx.bridge.remoteDefaultContact(), "", [ctx.bridge]);
  const contact = push?.contact || suggest;
  const set = (patch) => apply({ push: patch });
  const generated = status?.settings?.cert?.kind !== "files";
  return (
    <Row label={row.label} desc={row.desc} query={query} stack>
      <div className="st-rem">
        <_SRR_Line label="Enable push">
          <Toggle value={!!push?.enabled} label="Enable push notifications"
            onChange={v => apply(base => ({ push: v ? { enabled: true, contact: (base.push?.contact || suggest).trim() } : { enabled: false } }))} />
        </_SRR_Line>
        <_SRR_Line label="A question is waiting">
          <Toggle value={!!push?.questions} label="Notify on questions" onChange={v => set({ questions: v })} />
        </_SRR_Line>
        <_SRR_Line label="A turn finished">
          <Toggle value={!!push?.finished} label="Notify on finished turns" onChange={v => set({ finished: v })} />
        </_SRR_Line>
        <_SRR_Line label="Contact (VAPID)">
          <_SRR_Field value={contact} label="Push contact" placeholder="you@example.com" width={240} disabled={!push}
            onCommit={text => set({ contact: text.trim() })} />
        </_SRR_Line>
        {generated && (
          <div className="st-rem-warn">Push needs a certificate the phone trusts. A generated certificate is not trusted by default; use your own files.</div>
        )}
        <div className="st-btns">
          <Btn onClick={() => run(() => ctx.bridge.remotePushTest())} disabled={!push?.enabled}>Send test notification</Btn>
        </div>
        {status?.pushError && <div className="st-row-error">{status.pushError}</div>}
        {error && <div className="st-row-error">{error}</div>}
      </div>
    </Row>
  );
}

Object.assign(window.OMP_SETTINGS_ROWS = window.OMP_SETTINGS_ROWS || {}, {
  remoteServer: _SRR_Server,
  remoteCert: _SRR_Cert,
  remotePair: _SRR_Pair,
  remoteDevices: _SRR_Devices,
  remotePush: _SRR_Push,
});
