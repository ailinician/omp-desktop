#!/usr/bin/env python3
"""Minimal omp-desktop rail widget: echoes the context it was given."""
import json, os, sys
ctx = json.load(sys.stdin) if not sys.stdin.isatty() else {}
rows = [{"k": k, "v": str(ctx.get(k, os.environ.get(f"OMP_WIDGET_{k.upper()}", "")))}
        for k in ("project", "branch", "remote", "trigger")]
print(json.dumps({"version": 1, "status": "ok", "blocks": [{"type": "kv", "rows": rows}]}))
