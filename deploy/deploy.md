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
file only when it is missing; later edits stay put until you restart, and the
next `deploy.sh` run restarts phext-edit.

For the Google consent screen (Branding), use `https://edit.phext.io/about` as
the application home page and `https://edit.phext.io/privacy` as the privacy
policy link. Both are served by phext-edit itself and open without signing in.
Add `phext.io` under authorized domains. Terms are at `https://edit.phext.io/terms`.

## What deploy.sh does

Each run, in order:

1. Refuses to run as root. Checks that `SERVICE_USER` exists, `APP_DIR` is a
   git checkout, `PHEXT_DIR` exists, nginx is installed, and `sudo nginx -t`
   already succeeds. Nothing is written if that test fails.
2. Fetches `origin/BRANCH` (default `exo`) and hard-resets `APP_DIR` to it.
3. Runs `cargo build --release --bin phext-edit` as `SERVICE_USER`.
4. Creates `/etc/phext-edit/token.env` and `oauth.env` (mode `0600`, root-owned)
   only when each file is missing. An existing file is left as-is and is never
   printed. A token in an old unit's `--token` flag is copied into `token.env`
   once; otherwise a new token is generated.
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

### 3. Generate an auth token

```bash
TOKEN=$(openssl rand -hex 24)
echo "Your token: $TOKEN"
```

### 4. Systemd service (requires sudo)

```bash
# On mirrorborn.us:
sudo cp /source/phext-lattice/deploy/phext-edit.service /etc/systemd/system/
# Edit to set your token:
sudo sed -i "s/REPLACE_WITH_TOKEN/$TOKEN/" /etc/systemd/system/phext-edit.service
sudo systemctl daemon-reload
sudo systemctl enable phext-edit
sudo systemctl start phext-edit
# Verify:
sudo systemctl status phext-edit
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/status
```

### 4b. GitHub / Google sign-in (optional)

The token keeps working. Sign-in adds a second way in for the people you list.
Create `/etc/phext-edit/oauth.env` (mode `0600`, owned by root):

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

Signing in with GitHub or Google skips the token prompt. With `PHEXT_EDITORS`
empty, any account that signs in can read and edit every phext in the served
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

Enter your token to authenticate.

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
