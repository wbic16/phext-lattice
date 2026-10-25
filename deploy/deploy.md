# Deploying phext-edit to mirrorborn.us

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
# Who may edit once signed in: github:<numeric id>, google:<sub>, or a verified email
PHEXT_EDITORS=github:REPLACE_ME
GITHUB_CLIENT_ID=
GITHUB_CLIENT_SECRET=
GOOGLE_CLIENT_ID=
GOOGLE_CLIENT_SECRET=
ENV
sudo systemctl restart phext-edit
```

Signing in does not grant access by itself: the account must be in `PHEXT_EDITORS`.
A signed-in account that is not listed sees its `github:<id>` on the sign-in screen,
so you can copy it into the list. Every POST from a session also needs the CSRF
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
