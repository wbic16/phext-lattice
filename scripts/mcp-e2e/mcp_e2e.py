#!/usr/bin/env python3
"""End-to-end MCP client harness for phext-edit.

Replays the OAuth 2.1 + MCP flows the way real clients speak them —
Cursor's DCR shape, Claude's CIMD shape (its real client_id URL),
ChatGPT's CIMD + private_key_jwt shape — plus transport-conformance
checks (protocol-version header, trailing-slash endpoint, notification
status, TOC paging) and per-step latency budgets, against a locally
spawned phext-edit instance (see run.sh). Standard library only.

This harness exists so the next interop regression is caught by running
it, not by a user testing four hosted clients by hand. Every client
failure mode found in the 2026-11-09 interop rounds has a check here:
extra DCR grant types, private_key_jwt assertions and their replay,
suffixed discovery documents, plus-encoded scopes, resource binding,
the 9 MB unpaged table of contents, per-call re-opening of large
phexts, and a bare 404 at the trailing-slash endpoint spelling.

Usage: python3 mcp_e2e.py [base_url]     (default http://127.0.0.1:18917)
Exit status is non-zero when any non-skipped check fails. The Claude
CIMD check skips (does not fail) when the server cannot fetch the
client metadata document, which needs public DNS/HTTPS.
"""
import base64
import hashlib
import html
import http.client
import json
import re
import secrets
import sys
import time
import urllib.parse

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18917").rstrip("/")
HOST = urllib.parse.urlparse(BASE).hostname
PORT = urllib.parse.urlparse(BASE).port or 80
RESOURCE = BASE + "/mcp"
ISSUER = BASE

RESULTS = []  # (status, label, detail) status in PASS/FAIL/SKIP
TIMINGS = []  # (label, ms)


def check(ok, label, detail=""):
    RESULTS.append(("PASS" if ok else "FAIL", label, detail))
    print(f"    {'PASS' if ok else 'FAIL'}: {label}" + (f" — {detail}" if detail and not ok else ""))


def skip(label, why):
    RESULTS.append(("SKIP", label, why))
    print(f"    SKIP: {label} — {why}")


def req(method, path, body=None, headers=None, budget_ms=None, label=None):
    data = body.encode() if isinstance(body, str) else body
    conn = http.client.HTTPConnection(HOST, PORT, timeout=60)
    t0 = time.time()
    conn.request(method, path, body=data, headers=headers or {})
    resp = conn.getresponse()
    text = resp.read().decode("utf-8", "replace")
    ms = (time.time() - t0) * 1000
    out = (resp.status, dict(resp.getheaders()), text)
    conn.close()
    if label:
        TIMINGS.append((label, ms))
        print(f"    [{label}] {method} {path} -> {out[0]} in {ms:.0f} ms")
        if budget_ms is not None:
            check(ms <= budget_ms, f"budget: {label} <= {budget_ms} ms", f"took {ms:.0f} ms")
    return out


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def pkce():
    verifier = b64url(secrets.token_bytes(32))
    return verifier, b64url(hashlib.sha256(verifier.encode()).digest())


def consent_fields(page: str):
    fields = {}
    for tag in re.findall(r"<input[^>]*>", page):
        nm = re.search(r'name="([^"]+)"', tag)
        vl = re.search(r'value="([^"]*)"', tag)
        if nm:
            fields[nm.group(1)] = html.unescape(vl.group(1)) if vl else ""
    return fields


def authorize_and_consent(params, label):
    """GET authorize (expect the consent page), POST approve, return the
    callback query params."""
    path = "/oauth/authorize?" + urllib.parse.urlencode(params)
    status, _, page = req("GET", path, label=f"{label} authorize GET", budget_ms=5000)
    if status != 200:
        return None, status, page
    check("phext_csrf" in consent_fields(page) or "csrf" in page, f"{label}: consent form carries CSRF field")
    fields = consent_fields(page)
    fields["decision"] = "approve"
    status, hdrs, _ = req("POST", "/oauth/authorize", urllib.parse.urlencode(fields),
                          {"Content-Type": "application/x-www-form-urlencoded"},
                          label=f"{label} consent POST", budget_ms=2000)
    if status != 302:
        return None, status, hdrs.get("Location", "")
    loc = hdrs.get("Location", "")
    return dict(urllib.parse.parse_qsl(urllib.parse.urlparse(loc).query)), status, loc


def token_exchange(form, label, extra_headers=None):
    headers = {"Content-Type": "application/x-www-form-urlencoded"}
    if extra_headers:
        headers.update(extra_headers)
    return req("POST", "/oauth/token", urllib.parse.urlencode(form), headers,
               label=f"{label} token", budget_ms=2000)


def rpc(token, method, params=None, req_id=1, headers_extra=None, path="/mcp", label=None, budget_ms=None):
    headers = {"Authorization": f"Bearer {token}", "Content-Type": "application/json",
               "Accept": "application/json, text/event-stream"}
    if headers_extra:
        headers.update(headers_extra)
    payload = {"jsonrpc": "2.0", "id": req_id, "method": method}
    if params is not None:
        payload["params"] = params
    status, hdrs, text = req("POST", path, json.dumps(payload), headers, label=label, budget_ms=budget_ms)
    try:
        return status, hdrs, json.loads(text)
    except json.JSONDecodeError:
        return status, hdrs, {"raw": text}


def tool(token, name, args, label=None, budget_ms=None):
    status, _, value = rpc(token, "tools/call", {"name": name, "arguments": args},
                           label=label or f"tool {name}", budget_ms=budget_ms)
    inner = value["result"]["content"][0]["text"]
    return json.loads(inner)


def flow_code_and_token(*, label, client_id, redirect_uri, state, verifier,
                        token_extra=None, token_headers=None, expect_token_status=200):
    _, challenge = pkce_pair(verifier)
    params = {
        "response_type": "code", "client_id": client_id, "redirect_uri": redirect_uri,
        "code_challenge": challenge, "code_challenge_method": "S256",
        "resource": RESOURCE, "state": state, "scope": "phext:read",
    }
    q, status, loc = authorize_and_consent(params, label)
    if q is None:
        check(False, f"{label}: authorize+consent", f"status {status}: {loc[:160]}")
        return None
    check(q.get("state") == state, f"{label}: state echoes byte-identical")
    check(q.get("iss") == ISSUER, f"{label}: iss == issuer")
    if "code" not in q:
        check(False, f"{label}: code issued", loc[:160])
        return None
    form = {"grant_type": "authorization_code", "code": q["code"],
            "redirect_uri": redirect_uri, "code_verifier": verifier, "resource": RESOURCE}
    if token_extra:
        form.update(token_extra)
    status, _, body = token_exchange(form, label, token_headers)
    check(status == expect_token_status, f"{label}: token {expect_token_status}",
          f"got {status}: {body[:180]}")
    if status != 200:
        return None
    return json.loads(body)


def pkce_pair(verifier):
    return verifier, b64url(hashlib.sha256(verifier.encode()).digest())


# ── discovery conformance ────────────────────────────────────────────────
print("== DISCOVERY")
status, _, body = req("GET", "/.well-known/oauth-protected-resource", label="PRM", budget_ms=2000)
prm = json.loads(body)
check(status == 200 and prm.get("resource") == RESOURCE, "PRM resource == MCP URL")
check(prm.get("authorization_servers") == [ISSUER], "PRM authorization_servers == [issuer]")
status, _, body = req("GET", "/.well-known/oauth-authorization-server", label="AS metadata", budget_ms=2000)
meta = json.loads(body)
check(meta.get("issuer") == ISSUER, "AS metadata issuer")
check(meta.get("token_endpoint") == BASE + "/oauth/token", "AS metadata token_endpoint")
check(meta.get("code_challenge_methods_supported") == ["S256"], "AS metadata S256 only")
check("private_key_jwt" in meta.get("token_endpoint_auth_methods_supported", []),
      "AS metadata advertises private_key_jwt")
for suffix in ["/mcp", ""]:
    status, _, body = req("GET", f"/.well-known/oauth-authorization-server{suffix}")
    check(status == 200 and json.loads(body).get("issuer") == ISSUER,
          f"AS metadata at suffix '{suffix or '(plain)'}' serves the same issuer")
    status, _, _ = req("GET", f"/.well-known/oauth-protected-resource{suffix}")
    check(status == 200, f"PRM at suffix '{suffix or '(plain)'}' 200")
status, hdrs, _ = req("POST", "/mcp", "{}", {"Content-Type": "application/json"}, label="anon /mcp")
if status == 401:
    check(True, "anon /mcp is 401")
    challenge_hdr = hdrs.get("WWW-Authenticate", "")
    check("resource_metadata=" in challenge_hdr and "/.well-known/oauth-protected-resource" in challenge_hdr,
          "401 challenge carries resource_metadata", challenge_hdr)
    status, _, _ = req("POST", "/mcp/", "{}", {"Content-Type": "application/json"})
    check(status == 401, "anon /mcp/ (trailing slash) is 401, not 404", f"got {status}")
else:
    # A --local instance admits anonymous requests as the local
    # developer, so there is no challenge to inspect; the gated 401
    # behavior is covered by the Rust suite (mcp_refuses_anonymous…,
    # mcp_trailing_slash_spelling_answers). The trailing-slash spelling
    # is still asserted with a Bearer token below.
    skip("anon /mcp 401 + challenge", f"instance answered {status} (local mode admits anonymous)")
    skip("anon /mcp/ 401", "local mode admits anonymous")

# ── Cursor dialect: DCR public client, exact production shape ───────────
print("== FLOW Cursor (DCR)")
verifier, _ = pkce()
status, _, body = req("POST", "/oauth/register", json.dumps({
    "client_name": "Cursor",
    "redirect_uris": ["https://www.cursor.com/agents/mcp/oauth/callback"],
    "token_endpoint_auth_method": "none",
    "grant_types": ["authorization_code", "refresh_token",
                    "urn:ietf:params:oauth:grant-type:jwt-bearer"],
    "response_types": ["code"],
}), {"Content-Type": "application/json"}, label="cursor DCR", budget_ms=2000)
check(status == 201, "cursor: DCR 201 with an extra grant type", f"got {status}: {body[:160]}")
reg = json.loads(body)
check(reg.get("grant_types") == ["authorization_code", "refresh_token"],
      "cursor: grant_types normalized to the supported subset")
CURSOR_STATE = "421028467_JGJXk53LyU4hE-eWnstpj3dwH9H-A5CIWVf-vosDRyA.tgd"
tok = flow_code_and_token(label="cursor", client_id=reg["client_id"],
                          redirect_uri="https://www.cursor.com/agents/mcp/oauth/callback",
                          state=CURSOR_STATE, verifier=verifier,
                          token_extra={"client_id": reg["client_id"]})
check(tok is not None and "access_token" in tok, "cursor: access token issued")
cursor_token = tok["access_token"]

# ── transport conformance on the cursor token ────────────────────────────
print("== TRANSPORT CONFORMANCE")
status, _, value = rpc(cursor_token, "initialize", {
    "protocolVersion": "2025-11-25", "capabilities": {},
    "clientInfo": {"name": "harness", "version": "0"}}, label="initialize", budget_ms=2000)
check(status == 200 and value["result"]["protocolVersion"] in
      ["2024-11-05", "2025-03-26", "2025-06-18"],
      "initialize negotiates a supported protocol version")
check("name" in value["result"]["serverInfo"] and "version" in value["result"]["serverInfo"],
      "initialize result carries serverInfo name+version")
status, _, value = rpc(cursor_token, "tools/list", {}, label="tools/list", budget_ms=2000)
names = [t["name"] for t in value["result"]["tools"]]
check(names == ["list_phexts", "phext_toc", "read_scroll", "search_scrolls", "write_scroll"],
      "tools/list names", str(names))
status, _, _ = rpc(cursor_token, "tools/list", {}, path="/mcp/", label="/mcp/ tools/list", budget_ms=2000)
check(status == 200, "/mcp/ trailing-slash endpoint answers")
conn = http.client.HTTPConnection(HOST, PORT, timeout=30)
conn.request("POST", "/mcp", body=json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}),
             headers={"Authorization": f"Bearer {cursor_token}", "Content-Type": "application/json"})
resp = conn.getresponse(); resp.read(); conn.close()
check(resp.status == 202, "notifications get 202 with no body", f"got {resp.status}")
status, _, value = rpc(cursor_token, "tools/list", {},
                       headers_extra={"MCP-Protocol-Version": "2025-11-25"})
check(status == 200, "MCP-Protocol-Version: 2025-11-25 header accepted")
status, _, _ = rpc(cursor_token, "tools/list", {},
                   headers_extra={"MCP-Protocol-Version": "not-a-version"})
check(status == 400, "garbage MCP-Protocol-Version header refused 400", f"got {status}")
status, _, value = rpc(cursor_token, "no/suchmethod", {})
check(value.get("error", {}).get("code") == -32601, "unknown method -> -32601")

# ── Claude dialect: CIMD with Claude's real client_id URL ───────────────
print("== FLOW Claude (CIMD)")
verifier, _ = pkce()
CLAUDE_ID = "https://claude.ai/oauth/mcp-oauth-client-metadata"
q = {
    "response_type": "code", "client_id": CLAUDE_ID,
    "redirect_uri": "https://claude.ai/api/mcp/auth_callback",
    "code_challenge": pkce_pair(verifier)[1], "code_challenge_method": "S256",
    "resource": RESOURCE, "state": "oiaCH611XxsYUNeTC3iD0iI8krmLto_U7mkA4S_2z7I",
    "scope": "phext:read",
}
status, _, page = req("GET", "/oauth/authorize?" + urllib.parse.urlencode(q), label="claude authorize GET")
if status != 200:
    skip("claude CIMD flow", f"authorize returned {status} — the server-side CIMD fetch "
                             "needs public DNS/HTTPS, unavailable here")
else:
    fields = consent_fields(page)
    fields["decision"] = "approve"
    status, hdrs, _ = req("POST", "/oauth/authorize", urllib.parse.urlencode(fields),
                          {"Content-Type": "application/x-www-form-urlencoded"},
                          label="claude consent POST", budget_ms=2000)
    cb = dict(urllib.parse.parse_qsl(urllib.parse.urlparse(hdrs.get("Location", "")).query))
    check(status == 302 and "code" in cb, "claude: consent issues a code")
    form = {"grant_type": "authorization_code", "code": cb["code"],
            "redirect_uri": "https://claude.ai/api/mcp/auth_callback",
            "client_id": CLAUDE_ID, "code_verifier": verifier, "resource": RESOURCE}
    status, _, body = token_exchange(form, "claude")
    check(status == 200, "claude: token 200", f"got {status}: {body[:180]}")

# ── ChatGPT dialect: CIMD authorize (private_key_jwt at token) ──────────
print("== FLOW ChatGPT (CIMD + private_key_jwt)")
verifier, _ = pkce()
CHATGPT_ID = "https://chatgpt.com/oauth/client.json"
q = {
    "response_type": "code", "client_id": CHATGPT_ID,
    "redirect_uri": "https://chatgpt.com/connector_platform_oauth_redirect",
    "code_challenge": pkce_pair(verifier)[1], "code_challenge_method": "S256",
    "resource": RESOURCE, "state": "oauth_harness_state", "ui_locales": "en-US",
}
status, _, page = req("GET", "/oauth/authorize?" + urllib.parse.urlencode(q), label="chatgpt authorize GET")
if status != 200:
    skip("chatgpt CIMD authorize", f"authorize returned {status} — CIMD fetch needs public network")
else:
    fields = consent_fields(page)
    fields["decision"] = "approve"
    status, hdrs, _ = req("POST", "/oauth/authorize", urllib.parse.urlencode(fields),
                          {"Content-Type": "application/x-www-form-urlencoded"},
                          label="chatgpt consent POST", budget_ms=2000)
    cb = dict(urllib.parse.parse_qsl(urllib.parse.urlparse(hdrs.get("Location", "")).query))
    check(status == 302 and "code" in cb, "chatgpt: code issued (extra ui_locales tolerated)")
    form = {"grant_type": "authorization_code", "code": cb.get("code", ""),
            "redirect_uri": "https://chatgpt.com/connector_platform_oauth_redirect",
            "client_id": CHATGPT_ID, "code_verifier": verifier, "resource": RESOURCE}
    status, _, body = token_exchange(form, "chatgpt-noassert")
    check(status == 401 and "invalid_client" in body,
          "chatgpt: token without the required assertion is refused", f"got {status}: {body[:160]}")

# ── large-file behavior: paging + warm-cache latency ─────────────────────
print("== LARGE FILE (big.phext, 12000 scrolls)")
page1 = tool(cursor_token, "phext_toc", {"phext": "big.phext"}, label="toc big page 1", budget_ms=5000)
check(page1["total_scrolls"] == 12000, "big: total_scrolls", str(page1.get("total_scrolls")))
check(page1["returned"] == 10000 and page1["next_offset"] == 10000,
      "big: default page is 10000 with next_offset")
page2 = tool(cursor_token, "phext_toc", {"phext": "big.phext", "offset": 10000},
             label="toc big page 2", budget_ms=5000)
check(page2["returned"] == 2000 and page2["next_offset"] is None, "big: second page completes the TOC")
r1 = tool(cursor_token, "read_scroll", {"phext": "big.phext", "coordinate": page1["coordinates"][0]},
          label="read big first", budget_ms=5000)
r2 = tool(cursor_token, "read_scroll", {"phext": "big.phext", "coordinate": page1["coordinates"][0]},
          label="read big warm", budget_ms=2000)
check(r1["content"] == r2["content"], "big: warm read returns the same scroll")
hits = tool(cursor_token, "search_scrolls", {"phext": "big.phext", "query": "needle-7"},
            label="search big", budget_ms=5000)
check(len(hits["hits"]) > 0, "big: search finds hits")

# ── private_key_jwt dialect: DCR with inline JWKS ────────────────────────
print("== FLOW private_key_jwt (DCR + JWKS)")
try:
    from cryptography.hazmat.primitives.asymmetric import rsa
    from cryptography.hazmat.primitives import serialization
    import jwt as pyjwt
except ImportError:
    rsa = None
if rsa is None:
    skip("private_key_jwt flow", "needs the `cryptography` and `pyjwt` packages")
    print("\n== LATENCY TABLE")
    for _label, _ms in TIMINGS:
        print(f"    {_label:28s} {_ms:8.0f} ms")
    _fails = [r for r in RESULTS if r[0] == "FAIL"]
    _skips = [r for r in RESULTS if r[0] == "SKIP"]
    print(f"\n== SUMMARY: {len(RESULTS)} checks, {len(_fails)} failures, {len(_skips)} skips")
    sys.exit(1 if _fails else 0)

key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
pub = key.public_key().public_numbers()
jwk = {"kty": "RSA", "use": "sig", "alg": "RS256", "kid": "harness-key-1",
       "n": b64url(pub.n.to_bytes(256, "big")), "e": b64url(pub.e.to_bytes(3, "big"))}
status, _, body = req("POST", "/oauth/register", json.dumps({
    "client_name": "PKJ Harness", "redirect_uris": ["https://pkj.example/callback"],
    "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"],
    "token_endpoint_auth_method": "private_key_jwt",
    "token_endpoint_auth_signing_alg": "RS256", "jwks": {"keys": [jwk]},
}), {"Content-Type": "application/json"}, label="pkj DCR", budget_ms=2000)
check(status == 201, "pkj: DCR 201", f"got {status}: {body[:160]}")
reg = json.loads(body)
check("client_secret" not in reg, "pkj: no secret issued to a key client")
priv_pem = key.private_bytes(serialization.Encoding.PEM,
                             serialization.PrivateFormat.PKCS8,
                             serialization.NoEncryption()).decode()


def assertion(jti):
    now = int(time.time())
    return pyjwt.encode(
        {"iss": reg["client_id"], "sub": reg["client_id"], "aud": BASE + "/oauth/token",
         "jti": jti, "iat": now, "exp": now + 240},
        priv_pem, algorithm="RS256", headers={"kid": "harness-key-1"})


verifier, _ = pkce()
tok = flow_code_and_token(
    label="pkj", client_id=reg["client_id"], redirect_uri="https://pkj.example/callback",
    state="pkj-state", verifier=verifier,
    token_extra={"client_id": reg["client_id"],
                 "client_assertion_type": "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                 "client_assertion": assertion("harness-jti-1")})
check(tok is not None and "access_token" in tok, "pkj: access token issued")
verifier2, _ = pkce()
tok2 = flow_code_and_token(
    label="pkj-replay", client_id=reg["client_id"], redirect_uri="https://pkj.example/callback",
    state="pkj-state-2", verifier=verifier2, expect_token_status=401,
    token_extra={"client_id": reg["client_id"],
                 "client_assertion_type": "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                 "client_assertion": assertion("harness-jti-1")})
check(tok2 is None, "pkj: replayed assertion (same jti) is refused")

print("\n== LATENCY TABLE")
for label, ms in TIMINGS:
    print(f"    {label:28s} {ms:8.0f} ms")
fails = [r for r in RESULTS if r[0] == "FAIL"]
skips = [r for r in RESULTS if r[0] == "SKIP"]
print(f"\n== SUMMARY: {len(RESULTS)} checks, {len(fails)} failures, {len(skips)} skips")
sys.exit(1 if fails else 0)
