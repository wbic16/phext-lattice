# MCP end-to-end harness

Run the whole suite with one command, from the repository root:

```sh
bash scripts/mcp-e2e/run.sh
```

It builds `phext-edit` (release), spawns a throwaway instance in
`--local` mode over generated fixtures (a home phext plus a
12,000-scroll `big.phext`), and runs `mcp_e2e.py` against it. Exit
status is non-zero if any check fails. The Claude/ChatGPT CIMD checks
skip (not fail) when the sandbox has no public DNS/HTTPS for the
server-side client-metadata fetch; the `private_key_jwt` section skips
when the `cryptography`/`pyjwt` Python packages are unavailable.

## What it covers

- **Discovery conformance** — PRM and AS metadata at the plain and
  `/mcp`-suffixed well-known URLs, `WWW-Authenticate` challenge shape,
  unauthenticated GET and POST both returning the OAuth challenge,
  and the trailing-slash endpoint spelling answering 401 instead of 404.
- **Client dialects, replayed byte-for-byte from production logs** —
  Cursor's DCR request (including the extra `jwt-bearer` grant type
  that strict registration used to reject), Claude's CIMD flow with its
  real `client_id` URL and callback, ChatGPT's CIMD authorize (with the
  extra `ui_locales` parameter) and its assertion-less token refusal,
  and a full `private_key_jwt` DCR flow including assertion replay
  (`jti` reuse must fail).
- **Transport conformance** — initialize version negotiation,
  notifications answered 202 with no body, `MCP-Protocol-Version`
  header validation (published versions accepted, garbage refused
  400), unknown methods as `-32601`.
- **Large-file behavior + latency budgets** — `phext_toc` paging
  (default page 10,000 with `next_offset`), warm-cache reads and
  searches, and a per-step latency table with budgets (2 s for OAuth
  and small tool steps, 5 s for large-file steps on loopback).

`sdk_flow.py` is an optional second pass that drives the same server
with the official MCP Python SDK's `OAuthClientProvider`
(`pip install mcp`), proving the reference client completes the flow:

```sh
python3 scripts/mcp-e2e/sdk_flow.py http://127.0.0.1:18917
```

(against an instance started the way `run.sh` starts one).

## Why this exists

The 2026-11-09 interop rounds were debugged by a user hand-testing
Cursor, Claude, ChatGPT, Grok, and Antigravity against production.
Every failure mode found there — and every latency fix in the MCP
lattice cache — has a check here, so the next regression is caught by
running one command instead.
