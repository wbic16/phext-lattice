# Deploying phext-edit

`deploy/deploy.sh` is the only supported way to change how phext-edit is
installed on the edit.phext.io host. It is safe to rerun. A dry run that does
not touch the host is:

```bash
bash deploy/deploy.sh --check
```

A real run, from a sudo-capable account (not root):

```bash
ssh ubuntu@mc.phext.io
bash /source/phext-lattice/deploy/deploy.sh
```

Sign-in settings live in `/etc/phext-edit/oauth.env`. The callback URL is
`https://edit.phext.io/api/auth/<provider>/callback`. The script creates that
file only when it is missing. Later edits to the sign-in lines stay put. Each
run sets `PHEXT_SPONSOR_URL` in that file. When the variable is unset, the
run writes the live Payment Link (see below). The next `deploy.sh` run
restarts phext-edit.

For the Google consent screen (Branding), use `https://edit.phext.io/about` as
the application home page and `https://edit.phext.io/privacy` as the privacy
policy link. Both are served by phext-edit itself and open without signing in.
Add `phext.io` under authorized domains. Terms are at `https://edit.phext.io/terms`.

## Container deployment (additional path)

`deploy.sh` in this document is the supported path for the current
host. phext-edit can also run as a container under Docker Compose —
same binary, same environment contract, same data directory — as
documented in [CONTAINER.md](CONTAINER.md). The container path does
not replace `deploy.sh`: it reuses the nginx site `deploy.sh`
installs, and the two paths must not run at once against the same
port or the same data directory.

## Mutable OS single sign-on

Mutable (mutable.phext.io) takes its sign-in from here. A signed-in account
crosses via `GET /api/auth/sso/mutable` — linked as "Mutable OS" in the editor
shell's status bar — which redirects to Mutable's callback with a sealed,
120-second, single-use assertion. Signed-out visitors are sent to this site's
sign-in with the endpoint as the `next`, and land in Mutable right after.
Any signed-in account may cross; the assertion carries the account's editor
flag for Mutable to see.

Two lines in `/etc/phext-edit/oauth.env` turn it on:

```bash
PHEXT_SSO_SECRET=<generate once: openssl rand -base64 32>
# Optional; this is the default. A set-but-invalid value disables the handoff.
PHEXT_SSO_MUTABLE_URL=https://mutable.phext.io
```

`PHEXT_SSO_SECRET` is a dedicated key, never `PHEXT_SESSION_SECRET`: the same
value is set as `MUTABLE_SSO_SECRET` in Mutable's service environment, so
Mutable can verify assertions without ever holding a key that could mint
edit sessions. Without the secret, the SSO endpoint answers 404 and nothing
else changes.

## Exocortexia single sign-on

The Exocortexia suite crosses the same way, via
`GET /api/auth/sso/exocortexia`. The endpoint is the Mutable handoff
generalized by audience: the assertion format, the shared
`PHEXT_SSO_SECRET`, the 120-second single-use lifetime, and the
any-signed-in-account rule are all identical — only the sealed `aud`
claim (`exocortexia`) and the callback origin differ, and each consumer
rejects assertions minted for another audience. Audiences are
allowlisted in code (`mutable`, `exocortexia`, `wishnode`); anything else under
`/api/auth/sso/` answers 404, signed in or out.

One extra line in `/etc/phext-edit/oauth.env` turns it on:

```bash
# No default — until this is set to Exocortexia's real origin, the
# exocortexia endpoint answers 404. A set-but-invalid value also disables it.
PHEXT_SSO_EXOCORTEXIA_URL=https://<exocortexia origin>
```

No new secret is needed: the existing `PHEXT_SSO_SECRET` serves both
audiences, and Exocortexia's side is configured with that same value
under its own SSO-secret name. Deploy order matches Mutable's: this
side first (ship the build, set the URL line, restart phext-edit), then
enable Exocortexia's consumer side. Verify with a GET —
`curl -s -o /dev/null -w '%{http_code}' https://edit.phext.io/api/auth/sso/exocortexia`
answers 302 toward `/?next=…` when logged out, or toward Exocortexia's
callback with a session cookie. (Use GET, not HEAD: HEAD is not routed
to the gate and falls through to a 401.)

Deploy order matters in one direction: **this side first.** Ship the build
and add the secret, then `sudo systemctl restart phext-edit`. Verify with
`curl -sI https://edit.phext.io/api/auth/sso/mutable` — a 302 toward
`/?next=…` when logged out, or toward Mutable with a session cookie. Only
then enable the gate on Mutable; if Mutable redirects here before this
endpoint exists, its users hit a 404. No OAuth provider-app changes (the
registered callback URLs are untouched) and no nginx changes on either
vhost. To rotate the secret later, replace it on both sides and restart
both services in either order — assertions live at most 120 seconds, so
the skew costs at most one retried jump.

## Wishnode single sign-on

Wishnode (wishnode.net — the Exocortexia suite's public home) crosses
the same way, via `GET /api/auth/sso/wishnode`. Everything said about
Exocortexia above holds unchanged: the same assertion format, the same
shared `PHEXT_SSO_SECRET`, the same 120-second single-use lifetime, and
the same any-signed-in-account rule — only the sealed `aud` claim
(`wishnode`) and the callback origin differ, and Wishnode's consumer
rejects assertions minted for another audience. The allowlist in code
is (`mutable`, `exocortexia`, `wishnode`); anything else under
`/api/auth/sso/` answers 404, signed in or out. (A fourth audience,
`desktop`, is armed separately and works differently — see below.)

One extra line in `/etc/phext-edit/oauth.env` turns it on:

```bash
# No default — until this is set to Wishnode's real origin, the
# wishnode endpoint answers 404. A set-but-invalid value also disables it.
PHEXT_SSO_WISHNODE_URL=https://<wishnode origin>
```

No new secret is needed: the existing `PHEXT_SSO_SECRET` serves all
three audiences, and Wishnode's side is configured with that same value
under its own SSO-secret name. Deploy order matches Exocortexia's:
this side first (ship the build, set the URL line, restart phext-edit),
then enable Wishnode's consumer side. Verify with a GET —
`curl -s -o /dev/null -w '%{http_code}' https://edit.phext.io/api/auth/sso/wishnode`
answers 302 toward `/?next=…` when logged out, or toward Wishnode's
callback with a session cookie. (Use GET, not HEAD: HEAD is not routed
to the gate and falls through to a 401.)

## Desktop (Phext Notepad) single sign-on

The desktop audience serves Phext Notepad, which signs in to this
server without the user pasting the `phext_session` cookie. It works
differently from the web audiences because a shipped app cannot hold
`PHEXT_SSO_SECRET` — a shared secret inside a distributed binary is
public — so nothing is ever verified on the app's side:

1. The app opens a loopback listener and sends the browser to
   `GET /api/auth/sso/desktop?return=http://127.0.0.1:<port>/&state=<nonce>`.
   There is no target URL to configure: the return is per attempt, and
   only an `http` URL on the `127.0.0.1` or `[::1]` literal with an
   explicit port is admitted (hostnames — `localhost` included —
   `https`, and every other address are refused). The signed-out are
   bounced to sign-in with this endpoint as the `next`, as usual.
2. The mint redirects to the loopback return with a normal assertion —
   same sealed format, `aud` `desktop`, 120 seconds — plus the
   attempt's `state` nonce, which also rides sealed inside it.
3. The app POSTs the assertion and its state to
   `POST /api/auth/desktop/exchange` (headers `x-phext-assertion` /
   `x-phext-state`). This server verifies the seal, the audience, the
   expiry, and the state, burns the `jti` server-side (single-use is
   enforced here), checks the account against the editors list, and
   answers with a freshly sealed session value and its CSRF token as
   JSON — the credential the app stores and presents exactly like the
   cookie it replaces. A signed-in non-editor's assertion still mints,
   but the exchange answers it with the editor-list refusal and no
   credential.

One line in `/etc/phext-edit/oauth.env` arms it:

```bash
# Arming flag only — there is no desktop origin to configure (the
# return is the app's per-attempt loopback listener). Until this is
# set to exactly 1, both desktop endpoints answer 404.
PHEXT_SSO_DESKTOP=1
```

No new secret is needed: the existing `PHEXT_SSO_SECRET` seals the
assertions and never leaves this server. Ship the build, set the line,
restart phext-edit. Verify with a GET carrying a valid loopback return
and a state —
`curl -s -o /dev/null -w '%{http_code}' 'https://edit.phext.io/api/auth/sso/desktop?return=http://127.0.0.1:8912/&state=attempt-state-0123456789'`
answers 302 toward `/?next=…` while logged out (404 before the line is
set, 400 without a valid loopback return or state). Notepad builds
that know the flow fall back to the cookie paste while the endpoint
answers 404, so arming order does not strand older installs.

## Sponsor donation (no ads)

Phext Edit does not show advertisements, and it never will. There is no ad
slot, ad network, sponsored placement, or tracking pixel for an ad campaign.
The one offer on the site is a donation link, "Support the site", which opens
a Stripe Payment Link (Stripe's hosted Checkout) in a new tab. It is a
donation to support the site. It is not an advertisement and it is not the
Exocortex Founding purchase. Do not put that other Payment Link in this
variable.

The app does not call the Stripe API, does not embed Stripe.js, and does not
store a Stripe secret key. The live link is a public Payment Link, "Sponsor
edit.phext.io". The customer chooses the amount (minimum $1.00, preset $5.00).
It is not a secret, and it is the value `deploy.sh` writes when
`PHEXT_SPONSOR_URL` is unset:

```bash
PHEXT_SPONSOR_URL=https://donate.stripe.com/9B6dRa2t08tu7jUgxo5Vu0b
```

A normal deploy needs no extra variable:

```bash
bash /source/phext-lattice/deploy/deploy.sh
```

To point somewhere else for one run, export a different https URL first.
`deploy.sh` writes the value into `/etc/phext-edit/oauth.env`, which the
phext-edit service already loads, then restarts phext-edit. It changes only
the `PHEXT_SPONSOR_URL` line. Other lines are left as they are and are never
printed.

Set `PHEXT_SPONSOR_URL=` (empty) to leave a stored link in place, or to add an
empty key when the file does not have one yet. An empty value hides the link.
The editor reads the variable at startup and hides the control when it is
unset or empty, so local and dev runs that do not set it do not show a dead
link. The live URL is not compiled into the pages.

The value has to be one `https` URL with a host name and no spaces, quotes,
backslashes, or userinfo (`user@host`). `deploy.sh` refuses anything else.
The editor also hides a value it cannot place safely in the page.

Stripe's hosted confirmation page is the thank-you for this version, and
Stripe Dashboard receipts cover the receipt. This server does not handle a
webhook. A signed webhook handler for `checkout.session.completed` and
`checkout.session.async_payment_succeeded` — fulfilling only when
`payment_status` is not `unpaid` — is required before any post-donation side
effect (an email, a badge, access, or anything else that must happen because
someone paid) is treated as done.

## What deploy.sh does

Each run, in order:

1. Refuses to run as root. Checks that `SERVICE_USER` exists, `APP_DIR` is a
   git checkout, `PHEXT_DIR` exists, nginx is installed, and `sudo nginx -t`
   already succeeds. Nothing is written if that test fails.
2. Fetches `origin/BRANCH` (default `exo`) and hard-resets `APP_DIR` to it.
3. Runs `cargo build --release --bin phext-edit` as `SERVICE_USER`.
4. Creates `/etc/phext-edit/oauth.env` (mode `0600`, root-owned) only when
   missing. An existing file is left as-is and is never printed. (Access-token
   sign-in was retired in 2026-11; an existing `token.env` is vestigial and
   can be deleted — nothing reads it.) Every run then sets `PHEXT_SPONSOR_URL`.
   When that variable is unset, the live Payment Link
   `https://donate.stripe.com/9B6dRa2t08tu7jUgxo5Vu0b` is written. An explicit
   empty value adds the key when it is missing and does not erase a URL
   already stored. The value is not printed.
5. Writes `/etc/systemd/system/phext-edit.service` (a timestamped `.bak-*` copy
   is kept when the unit changes), `systemctl daemon-reload`, enables the
   service, and **restarts phext-edit**. It waits until `http://127.0.0.1:PORT/`
   answers. The restart is what picks up a new binary; nginx is reloaded only
   after that, so the proxy is aimed at the process this run just started.
6. Reconciles `/etc/nginx/sites-available/DOMAIN` from
   `deploy/nginx-edit.phext.io.conf` once
   `/etc/letsencrypt/live/DOMAIN` exists. Before the certificate exists it
   writes a short HTTP bootstrap (also marked as managed) so certbot has a
   server block, runs certbot when DNS points at `EXPECTED_A`, then installs
   the hardened file. The hardened file is what sets `server_tokens off`, the
   port-80 `return 301`, `Strict-Transport-Security`, and both `limit_req`
   zones. `sudo nginx -t` runs before `systemctl reload nginx`. If either
   fails, the previous site file is restored and the script exits non-zero.
   A reload that fails is followed by another reload of the restored file.
7. When the certificate is in place and `--no-redirect` is not set, points
   `OLD_SITE` (`mirrorborn.us`) `/editor` at `https://DOMAIN/`, including
   `Clear-Site-Data`, and reloads nginx only after `nginx -t`.

`DOMAIN` and `PORT` in the template (`edit.phext.io`, `127.0.0.1:8080`) are
replaced from the script's domain and port. `ssl_certificate` and
`ssl_certificate_key` already present in the live site file are copied into
the rendered file. An `ssl_dhparam` line is omitted when that file is not on
the host.

## Share image and icons

`/favicon.ico`, `/favicon.svg`, `/apple-touch-icon.png` (and the precomposed
alias), and `/og.png` are compiled into the phext-edit binary from `web/`
(`include_bytes!` in `src/bin/phext_edit.rs`). Step 3 builds that binary and
step 5 restarts the service, which is what publishes them. The nginx site
proxies `location /` to the app, so these paths are not files copied onto the
host and the site file does not grow a root or an alias for them. A normal
`deploy.sh` run is the whole update. `/robots.txt` and `/sitemap.xml` are the
same kind of compiled-in response; they are not separate files on disk.

## Managed-file marker

The first line of a site file this script writes is:

```text
# phext-lattice-managed: kind=site file=deploy/nginx-edit.phext.io.conf template=<sha256> body=<sha256>
```

`body` is the SHA-256 of every byte after that line. The HTTP bootstrap uses
`kind=http-bootstrap` and the same `body=` hash.

On a later run:

- The same bytes: leave the file, ensure the `sites-enabled` symlink, do not
  reload.
- Marker present and `body` still matches the file, but the rendered config
  differs: this script owns it. Back up to `sites-available/DOMAIN.bak-<timestamp>`,
  install the new file, `nginx -t`, reload.
- Marker present but `body` does not match (someone edited the file and left
  the marker): refuse.
- No marker: refuse.
- First line is exactly `# Managed by phext-lattice deploy/deploy.sh.` (the
  bootstrap written by older revisions of this script): treat it as owned and
  replace it. Certificate paths in that file are kept.

A refused run exits non-zero after phext-edit has already been restarted. The
live nginx file is not modified.

## Latches and overrides

Environment variables override the defaults. Flags override the same names.

| Latch | Default | Effect |
| --- | --- | --- |
| `BRANCH` / `--branch` | `exo` | Git branch that is hard-reset and built |
| `PORT` / `--port` | `8080` | phext-edit listen port and nginx `proxy_pass` |
| `DOMAIN` / `--domain` | `edit.phext.io` | nginx `server_name`, certificate directory, site file name |
| `APP_DIR` | `/source/phext-lattice` | Checkout that is reset and built |
| `PHEXT_DIR` | `/source/human` | Directory passed to `--dir` |
| `PHEXT_HOME` | `choose-your-own-adventure.phext` | `--home` file |
| `PHEXT_HOME_COORD` | `10.10.2/1.1.1/1.10.10` | `--home-coord` |
| `SERVICE_USER` | `wbic16` | User that owns the checkout and the service process |
| `EXPECTED_A` | `44.248.235.76` | Address `DOMAIN` must resolve to before certbot |
| `OLD_SITE` | `mirrorborn.us` | Vhost whose `/editor` locations are redirected |
| `NO_TLS=1` / `--no-tls` | off | Do not run certbot |
| `NO_REDIRECT=1` / `--no-redirect` | off | Do not edit the `OLD_SITE` vhost |
| `PHEXT_NGINX_FORCE=1` / `--force-nginx` | off | Replace a site file the marker does not own. The previous file is copied to `DOMAIN.bak-<timestamp>` first. Any value other than `1` does not force. |
| `--check`, `--dry-run` | off | Render the nginx site, check the marker hash, and exercise the latch against temp files. No sudo, no build, no restart, no write under `/etc`. |

To keep a hand-edited site file, do not set the force latch. Remove a legacy
`# Managed by phext-lattice deploy/deploy.sh.` first line first if that comment
is still there; otherwise the next run still treats the file as owned.

`--check` also checks that the rendered port-80 server only redirects, and
that `server_tokens off`, HSTS, and both `limit_req` zones survived rendering.

## After a run

```bash
curl -sI https://edit.phext.io/ | sed -n '1,40p'
# 200, cache-control public, max-age=60, must-revalidate, etag, CSP,
# nosniff, X-Frame-Options DENY, and strict-transport-security max-age=31536000.
# No access-control-allow-origin line.

ETAG=$(curl -sI https://edit.phext.io/ | awk -F': ' 'tolower($1)=="etag"{print $2}' | tr -d '\r')
curl -sI -H "If-None-Match: $ETAG" https://edit.phext.io/ | head -n 1
# 304

curl -sI http://edit.phext.io/ | head -n 5
# 301 to https://edit.phext.io/

curl -sI https://edit.phext.io/ | grep -i '^server:'
# server: nginx   (no version)

sudo certbot renew --dry-run
```

If `nginx -t` fails because `phext_edit_req` or `phext_auth` is already defined
in another file, deploy.sh restores the previous site file and stops. A zone
name may be defined once. The duplicate has to be removed from that other file
and `deploy.sh` rerun; the script will not leave the failed file in place.

## Historical: mirrorborn.us/editor

The steps below describe the previous layout, where the editor was a path on
`mirrorborn.us`. They are not a deploy procedure. `deploy.sh` now owns the
`/editor` redirect on that vhost. Do not copy unit files or nginx snippets
beside what `deploy.sh` writes.

## mirrorborn.us/editor (previous setup)

## Architecture

```
Browser → mirrorborn.us (nginx on AWS)
           ├── /editor/           → static HTML (site repo)
           └── /editor/api/*      → proxy to phext-edit on :8080
```

## Steps

### 1. Binary (already deployed)

The binary was built on aletheia-core and scp'd to:
```
/source/phext-lattice/target/release/phext-edit
```

To update:
```bash
# On aletheia-core:
cd /source/phext-lattice && cargo build --release -p lattice-ui
scp target/release/phext-edit mirrorborn.us:/source/phext-lattice/target/release/phext-edit
```

### 2. Phext files

The `/source/human` repo was cloned via HTTPS. To update:
```bash
ssh mirrorborn.us "cd /source/human && git pull"
```

### 3. Systemd service (requires sudo)

```bash
# On mirrorborn.us:
sudo cp /source/phext-lattice/deploy/phext-edit.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable phext-edit
sudo systemctl start phext-edit
# Verify (the session endpoint answers while signed out):
sudo systemctl status phext-edit
curl -s http://localhost:8080/api/session
```

### 4. GitHub / Google sign-in

Sign-in is the only way in, for the people you list — access-token
sign-in was retired. Create `/etc/phext-edit/oauth.env` (mode `0600`,
owned by root):

```bash
sudo mkdir -p /etc/phext-edit
sudo install -m 600 /dev/null /etc/phext-edit/oauth.env
sudo tee /etc/phext-edit/oauth.env >/dev/null <<ENV
PHEXT_PUBLIC_URL=https://mirrorborn.us/editor
PHEXT_SESSION_SECRET=$(openssl rand -hex 32)
PHEXT_COOKIE_SECURE=1
# Empty: any signed-in account gets in. To restrict: github:<numeric id>,
# google:<sub>, or a verified email, comma-separated.
PHEXT_EDITORS=
GITHUB_CLIENT_ID=
GITHUB_CLIENT_SECRET=
GOOGLE_CLIENT_ID=
GOOGLE_CLIENT_SECRET=
ENV
sudo systemctl restart phext-edit
```

With `PHEXT_EDITORS` empty, any account that signs in can read and edit every phext in the served
directory. To restrict it, list the accounts; one that is not listed sees its
`github:<id>` on the sign-in screen, so you can copy it into the list. Every POST from a session also needs the CSRF
token from `/api/session` (the UI handles this).

GitHub: Settings → Developer settings → OAuth Apps → New OAuth App. Homepage
`https://mirrorborn.us/editor`. Callback URL
`https://mirrorborn.us/editor/api/auth/github/callback`.

Google: Cloud Console → APIs & Services → Credentials → OAuth client ID → Web
application. Authorized redirect URI
`https://mirrorborn.us/editor/api/auth/google/callback`. Scopes: `openid`,
`email`, `profile`.

The auth routes live under `/api/auth/`, so the existing `/editor/api/` proxy
already reaches them. The startup log lists which methods are active.

### 5. Nginx config

Add to the HTTPS server block in `/etc/nginx/sites-enabled/mirrorborn.us`:

```nginx
    # Phext Editor — reverse proxy to phext-edit on :8080
    location /editor/api/ {
        proxy_pass http://127.0.0.1:8080/api/;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    location /editor/ {
        try_files $uri $uri/ /editor/index.html;
    }
```

Then:
```bash
sudo nginx -t && sudo systemctl reload nginx
```

### 6. Deploy the site

```bash
cd /source/site-mirrorborn-us && git pull
# Site files are in /sites/web/mirrorborn.us/ — sync if needed:
rsync -av /source/site-mirrorborn-us/editor/ /sites/web/mirrorborn.us/editor/
```

### 7. Visit

https://mirrorborn.us/editor

Sign in with GitHub or Google to authenticate.

## Updating phexts

```bash
ssh mirrorborn.us "cd /source/human && git pull"
sudo systemctl restart phext-edit
```

## Updating the binary

```bash
# Build on aletheia-core
cd /source/phext-lattice && cargo build --release -p lattice-ui
scp target/release/phext-edit mirrorborn.us:/source/phext-lattice/target/release/
ssh mirrorborn.us "sudo systemctl restart phext-edit"
```
