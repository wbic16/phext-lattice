#!/usr/bin/env python3
"""Drive the official MCP Python SDK (OAuthClientProvider) through the full
discovery -> DCR -> authorize -> consent -> token -> initialize -> tools flow
against a locally running phext-edit instance (--local mode, so the consent
step needs no cookies). Prints per-step timings.

Usage: sdk_flow.py [base_url]   (default http://127.0.0.1:8917)
"""
import anyio
import html
import re
import sys
import time
import urllib.parse

import httpx2 as httpx
from pydantic import AnyUrl

from mcp.client.session import ClientSession
from mcp.client.streamable_http import streamable_http_client as streamablehttp_client
from mcp.client.auth import OAuthClientProvider
from mcp.shared.auth import OAuthClientMetadata, AuthorizationCodeResult

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8917").rstrip("/")
MCP_URL = BASE + "/mcp"

holder = {}
t0 = time.time()


def stamp(msg):
    print(f"[{time.time() - t0:6.2f}s] {msg}", flush=True)


class MemoryStorage:
    def __init__(self):
        self._tokens = None
        self._client = None

    async def get_tokens(self):
        return self._tokens

    async def set_tokens(self, tokens):
        self._tokens = tokens

    async def get_client_info(self):
        return self._client

    async def set_client_info(self, client_info):
        self._client = client_info


async def redirect_handler(authorization_url: str) -> None:
    stamp(f"redirect_handler: authorize URL received")
    async with httpx.AsyncClient(follow_redirects=False, timeout=30) as client:
        r = await client.get(authorization_url)
        assert r.status_code == 200, f"consent GET: {r.status_code} {r.text[:200]}"
        fields = {}
        for tag in re.findall(r"<input[^>]*>", r.text):
            nm = re.search(r'name="([^"]+)"', tag)
            vl = re.search(r'value="([^"]*)"', tag)
            if nm:
                fields[nm.group(1)] = html.unescape(vl.group(1)) if vl else ""
        fields["decision"] = "approve"
        r2 = await client.post(BASE + "/oauth/authorize", data=fields)
        assert r2.status_code == 302, f"consent POST: {r2.status_code} {r2.text[:200]}"
        loc = r2.headers["location"]
        q = dict(urllib.parse.parse_qsl(urllib.parse.urlparse(loc).query))
        assert "code" in q, f"no code in callback: {loc}"
        holder["code"] = q["code"]
        holder["state"] = q.get("state")
    stamp("redirect_handler: consent approved, code captured")


async def callback_handler() -> AuthorizationCodeResult:
    return AuthorizationCodeResult(code=holder["code"], state=holder.get("state"))


async def main():
    metadata = OAuthClientMetadata(
        client_name="MCP Python SDK replay",
        redirect_uris=[AnyUrl("http://127.0.0.1:8917/sdk-callback")],
        grant_types=["authorization_code", "refresh_token"],
        token_endpoint_auth_method="none",
    )
    provider = OAuthClientProvider(
        server_url=MCP_URL,
        client_metadata=metadata,
        storage=MemoryStorage(),
        redirect_handler=redirect_handler,
        callback_handler=callback_handler,
    )
    http_client = httpx.AsyncClient(auth=provider, timeout=httpx.Timeout(120.0, connect=15.0))
    async with streamablehttp_client(MCP_URL, http_client=http_client) as (read, write):
        async with ClientSession(read, write) as session:
            t = time.time()
            init = await session.initialize()
            stamp(f"initialize OK ({time.time() - t:.2f}s): server={init.server_info.name} protocol={init.protocol_version}")
            t = time.time()
            tools = await session.list_tools()
            stamp(f"tools/list OK ({time.time() - t:.2f}s): {[tool.name for tool in tools.tools]}")
            t = time.time()
            res = await session.call_tool("list_phexts", {})
            stamp(f"list_phexts OK ({time.time() - t:.2f}s): {res.content[0].text[:110]}")
            t = time.time()
            res = await session.call_tool("phext_toc", {"phext": "ultra.phext"})
            text = res.content[0].text
            stamp(f"phext_toc ultra OK ({time.time() - t:.2f}s): {len(text)} chars, head: {text[:80]!r}")
            t = time.time()
            res = await session.call_tool("phext_toc", {"phext": "ultra.phext"})
            stamp(f"phext_toc ultra #2 OK ({time.time() - t:.2f}s)")
            t = time.time()
            res = await session.call_tool("search_scrolls", {"phext": "ultra.phext", "query": "the"})
            stamp(f"search ultra 'the' OK ({time.time() - t:.2f}s): {res.content[0].text[:110]}")
    stamp("SDK FLOW COMPLETE")


if __name__ == "__main__":
    anyio.run(main)
