/* ═════════════════════════════════════════════════════════════════════
   design/settings/rows-remote-shared.jsx — shared state, hooks and field
   controls for the Remote access Settings rows (`rows-remote-server.jsx`,
   `rows-remote-phones.jsx`). Rust owns `remote.json`; every row shows
   `remoteStatus()` (read on mount, refreshed on `remote://status`) and
   saves by sending the COMPLETE settings object to `remoteApply`.
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

/** Text field committed on blur or Enter; re-syncs when the saved value changes.
 *  `onCommit` returning `false` rejects the text: the field shows the saved value again. */
function _SRR_Field({ value, onCommit, label, placeholder, width, disabled, mono = true }) {
  const [text, setText] = React.useState(value);
  React.useEffect(() => setText(value), [value]);
  const commit = () => { if (text !== value && onCommit(text) === false) setText(value); };
  return (
    <input className="st-rem-input" type="text" value={text} aria-label={label} placeholder={placeholder}
      disabled={disabled} spellCheck={false} style={{ width, fontFamily: mono ? undefined : "inherit" }}
      onChange={e => setText(e.target.value)} onBlur={commit}
      onKeyDown={e => {
        // Enter confirms an IME candidate; only a plain Enter commits.
        if (e.key !== "Enter" || e.nativeEvent?.isComposing || e.keyCode === 229) return;
        e.currentTarget.blur();
      }} />
  );
}

function _SRR_Line({ label, children }) {
  return <div className="st-rem-line"><span className="st-rem-lbl">{label}</span>{children}</div>;
}
