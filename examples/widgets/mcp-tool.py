#!/usr/bin/env python3
"""omp-desktop rail widget that calls one MCP tool over stdio and shows its
text result. Run with
`uv run --directory ~/.omp-desktop/widgets --no-project --no-config --with mcp mcp-tool.py`
or install `mcp`. `--directory` keeps uv out of the untrusted project: without
it uv still runs the interpreter of a `.venv` found in the project or any of
its parents, even with `--no-project`. `--no-project`/`--no-config` keep it
from reading pyproject.toml, uv.toml and .python-version. Edit SERVER and
TOOL in main(); arguments come from the widget context. Contract:
docs/widgets.md."""
import asyncio, json, os, sys

async def main():
    from mcp import ClientSession, StdioServerParameters
    from mcp.client.stdio import stdio_client

    # cwd = home, not the project: npx would otherwise read the project's
    # .npmrc and prefer its node_modules.
    SERVER = StdioServerParameters(command="npx", args=["-y", "@modelcontextprotocol/server-everything"],
                                   cwd=os.path.expanduser("~"))
    TOOL, ARGS = "echo", {"message": f"branch {os.environ.get('OMP_WIDGET_BRANCH','')}"}

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
