#!/usr/bin/env python3
"""omp-desktop rail widget that calls one MCP tool over stdio and shows its
text result. Run with `uv run --with mcp mcp-tool.py` or install `mcp`.
Edit SERVER and TOOL below; arguments come from the widget context."""
import asyncio, json, os, sys
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

SERVER = StdioServerParameters(command="npx", args=["-y", "@modelcontextprotocol/server-everything"])
TOOL, ARGS = "echo", {"message": f"branch {os.environ.get('OMP_WIDGET_BRANCH','')}"}

async def main():
    async with stdio_client(SERVER) as (r, w):
        async with ClientSession(r, w) as s:
            await s.initialize()
            res = await s.call_tool(TOOL, ARGS)
            text = "\n".join(c.text for c in res.content if getattr(c, "type", "") == "text")
            print(json.dumps({"version": 1, "status": "error" if res.isError else "ok",
                              "blocks": [{"type": "text", "text": text or "(no text content)"}]}))

try:
    asyncio.run(main())
except Exception as e:  # any failure becomes a readable error card
    print(json.dumps({"version": 1, "status": "error", "blocks": [{"type": "text", "text": f"{type(e).__name__}: {e}"}]}))
