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
