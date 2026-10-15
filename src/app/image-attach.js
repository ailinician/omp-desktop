/* ═════════════════════════════════════════════════════════════════════
   app/image-attach.js — turn picked/pasted/dropped image Files (and the
   browser panel's annotation screenshots) into omp `ImageContent` blocks
   ({ type: "image", data: <base64>, mimeType }), the shape the `prompt` /
   `steer` / `follow_up` RPC commands accept.
   ═════════════════════════════════════════════════════════════════════ */
(function () {
  // Formats every omp provider takes as-is. Anything else a clipboard or
  // file picker can hand us (bmp, tiff, svg, …) is re-encoded via canvas.
  const PASSTHROUGH_MIME = new Set(["image/png", "image/jpeg", "image/gif", "image/webp"]);

  // A client-side sanity cap, not a match for any particular provider's
  // vision-processing limit — kept modest so a raw multi-MB screenshot or
  // photo doesn't sit around as an oversized base64 string client-side
  // before omp's own ingestion-time resize (`resizeImage`, run when the
  // prompt/steer/follow_up command is received) normalizes it further.
  const MAX_EDGE_PX = 2000;

  // Re-encode threshold. omp resizes/re-encodes every incoming image itself
  // on ingestion (it does not persist or echo attachments verbatim), so this
  // isn't guarding the RPC transport — protocol v2 chunks any oversized
  // stdout object into ≤1 MiB `rpc_chunk` frames well below
  // `agent/reader.rs::MAX_LINE_BYTES` regardless of attachment size. It only
  // keeps what this app base64-encodes and holds in memory/IPC reasonable.
  const MAX_PASSTHROUGH_BYTES = 1536 * 1024;
  const JPEG_QUALITY = 0.85;
  const MAX_ATTACHMENTS = 8;

  function parseDataUrl(url) {
    const m = /^data:(image\/[^;,]+);base64,([A-Za-z0-9+/=]+)$/.exec(url ?? "");
    return m ? { type: "image", mimeType: m[1], data: m[2] } : null;
  }

  function toDataUrl(img) {
    return `data:${img.mimeType};base64,${img.data}`;
  }

  // Decoded size of a base64 payload, without decoding it.
  function decodedSize(b64) {
    return Math.floor((b64.length * 3) / 4);
  }

  function fitWithin(width, height, max = MAX_EDGE_PX) {
    if (width <= max && height <= max) return { width, height };
    const scale = max / Math.max(width, height);
    return {
      width:  Math.max(1, Math.round(width * scale)),
      height: Math.max(1, Math.round(height * scale)),
    };
  }

  function needsReencode(mimeType, bytes, width, height) {
    return !PASSTHROUGH_MIME.has(mimeType)
      || bytes > MAX_PASSTHROUGH_BYTES
      || width > MAX_EDGE_PX
      || height > MAX_EDGE_PX;
  }

  // During dragenter/dragover only `types` is readable — the data itself
  // stays hidden until the drop. `types` is an array in current engines
  // and a DOMStringList in older WebKit; Array.from reads both.
  function carriesType(dt, type) {
    return Array.from(dt?.types ?? []).includes(type);
  }

  // Whether a drag carries files from outside the page (the OS file
  // manager). A text selection dragged inside the app carries no "Files",
  // so it keeps the browser's own handling.
  function carriesFiles(dt) {
    return carriesType(dt, "Files");
  }

  // Whether a drag no handler took must be refused, so the webview's own
  // drop never navigates to it (navigation_guard.rs would open a dropped
  // link in the system browser). A file is refused everywhere; a link
  // (`text/uri-list`) only off an editable field, which inserts it as text.
  function refusesStrayDrop(dt, onEditable) {
    return carriesFiles(dt) || (!onEditable && carriesType(dt, "text/uri-list"));
  }

  // The element a file drag is over inside the drop zone, after one
  // dragenter/dragleave/drop that reached the zone (`null`: not over it).
  // Engines fire dragenter on the new element before dragleave on the old
  // one, so a leave only ends the hover when it leaves the element last
  // entered, i.e. the next one is outside the zone. Unlike a depth count,
  // this survives a leave that never arrives: one whose element was
  // removed mid-drag goes to the detached node, not to the zone.
  function dropHoverAfter(current, type, target) {
    if (type === "dragenter") return target;
    if (type === "dragleave") return target === current ? null : current;
    return null;
  }

  // Image Files carried by a paste/drop DataTransfer. Engines differ on
  // whether a pasted screenshot shows up in `items`, `files`, or both, so
  // read `items` and fall back to `files` only when it yielded nothing —
  // reading both would attach the same image twice where both are filled.
  function imageFilesFromTransfer(dt) {
    if (!dt) return [];
    const out = [];
    for (const item of Array.from(dt.items ?? [])) {
      if (item.kind !== "file" || !item.type.startsWith("image/")) continue;
      const file = item.getAsFile();
      if (file) out.push(file);
    }
    if (out.length > 0) return out;
    return Array.from(dt.files ?? []).filter((f) => f.type.startsWith("image/"));
  }

  // WebKitGTK (this app's Linux Tauri target) never populates image/*
  // entries in the synchronous `paste` event's DataTransfer — `items`
  // and `files` both come back carrying only text/* even though
  // `javascript-can-access-clipboard` is enabled and the key event is a
  // genuine compositor-level Ctrl+V (verified live against WebKitGTK
  // 2.52). The async Clipboard API does see it there, so this is a
  // fallback for when the synchronous extraction above finds nothing —
  // Chromium/WebKit(macOS)/WebView2 already populate the synchronous
  // path and never need it.
  async function imageFilesFromClipboardAsync() {
    if (!navigator.clipboard?.read) return [];
    let items;
    try {
      items = await navigator.clipboard.read();
    } catch {
      return []; // no permission, or nothing readable — not an error the caller should surface
    }
    const out = [];
    for (const item of items) {
      const type = item.types.find((t) => t.startsWith("image/"));
      if (!type) continue;
      try {
        const blob = await item.getType(type);
        out.push(new File([blob], `clipboard.${type.split("/")[1] || "png"}`, { type }));
      } catch {
        // unreadable item — skip, don't drop the rest of the clipboard
      }
    }
    return out;
  }

  function readAsDataUrl(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload  = () => resolve(reader.result);
      reader.onerror = () => reject(reader.error ?? new Error("could not read file"));
      reader.readAsDataURL(file);
    });
  }

  function decode(src) {
    return new Promise((resolve, reject) => {
      const img = new Image();
      img.onload  = () => resolve(img);
      img.onerror = () => reject(new Error("not a decodable image"));
      img.src = src;
    });
  }

  // A data URL (`bytes` long decoded) → ImageContent, downscaled/re-encoded
  // when it would be too large or in a format a provider may reject.
  // Rejects on undecodable input.
  async function normalizeDataUrl(url, bytes) {
    const img = await decode(url);
    const w = img.naturalWidth;
    const h = img.naturalHeight;
    const original = parseDataUrl(url);
    if (original && !needsReencode(original.mimeType, bytes, w, h)) return original;

    const { width, height } = fitWithin(w, h);
    const canvas = document.createElement("canvas");
    canvas.width  = width;
    canvas.height = height;
    const ctx = canvas.getContext("2d");
    ctx.drawImage(img, 0, 0, width, height);
    // PNG keeps transparency and text crispness (screenshots); fall back to
    // JPEG only when PNG is still too heavy (photos), flattening any alpha
    // onto white first — JPEG would otherwise turn it black.
    let out = parseDataUrl(canvas.toDataURL("image/png"));
    if (!out || decodedSize(out.data) > MAX_PASSTHROUGH_BYTES) {
      ctx.globalCompositeOperation = "destination-over";
      ctx.fillStyle = "#fff";
      ctx.fillRect(0, 0, width, height);
      out = parseDataUrl(canvas.toDataURL("image/jpeg", JPEG_QUALITY));
    }
    if (!out) throw new Error("could not encode image");
    return out;
  }

  // File → ImageContent (see normalizeDataUrl).
  async function prepareImage(file) {
    return normalizeDataUrl(await readAsDataUrl(file), file.size);
  }

  // An ImageContent from elsewhere (a browser panel screenshot, captured at
  // the page's device pixel ratio) through the same edge cap and re-encode
  // as a pasted image.
  function normalizeImage(img) {
    return normalizeDataUrl(toDataUrl(img), decodedSize(img.data));
  }

  window.OMP_IMAGES = {
    MAX_ATTACHMENTS,
    parseDataUrl,
    toDataUrl,
    fitWithin,
    needsReencode,
    carriesFiles,
    refusesStrayDrop,
    dropHoverAfter,
    imageFilesFromTransfer,
    imageFilesFromClipboardAsync,
    prepareImage,
    normalizeImage,
  };
})();
