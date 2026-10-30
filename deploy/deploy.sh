#!/usr/bin/env bash
# Deploy phext-edit at edit.phext.io with the nginx already on this host.
# Idempotent: run it again to rebuild exo and restart.
#
#   bash deploy/deploy.sh [--no-tls] [--no-redirect] [--domain edit.phext.io]
#
# Run as a sudo-capable account (ubuntu). The checkout, build, and service
# belong to SERVICE_USER (wbic16). Secrets live in /etc/phext-edit/*.env,
# mode 0600, and are never printed.
#
# What it touches:
#   /etc/systemd/system/phext-edit.service   (backed up when it changes)
#   /etc/phext-edit/token.env, oauth.env     (created only when missing)
#   /etc/nginx/sites-{available,enabled}/DOMAIN
#   /etc/nginx/sites-available/OLD_SITE      (only the /editor locations,
#                                             once DOMAIN has TLS)
set -euo pipefail

BRANCH="${BRANCH:-exo}"
PORT="${PORT:-8080}"
DOMAIN="${DOMAIN:-edit.phext.io}"
PHEXT_HOME="${PHEXT_HOME:-choose-your-own-adventure.phext}"
PHEXT_HOME_COORD="${PHEXT_HOME_COORD:-10.10.2/1.1.1/1.10.10}"
EXPECTED_A="${EXPECTED_A:-44.248.235.76}"
APP_DIR="${APP_DIR:-/source/phext-lattice}"
PHEXT_DIR="${PHEXT_DIR:-/source/human}"
SERVICE_USER="${SERVICE_USER:-wbic16}"
OLD_SITE="${OLD_SITE:-mirrorborn.us}"
NO_TLS="${NO_TLS:-0}"
NO_REDIRECT="${NO_REDIRECT:-0}"

SERVICE="phext-edit.service"
UNIT_PATH="/etc/systemd/system/${SERVICE}"
CONF_DIR="/etc/phext-edit"

die() {
  printf 'deploy.sh: %s\n' "$*" >&2
  exit 1
}

usage() {
  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
}

random_hex() {
  od -An -N32 -tx1 /dev/urandom | tr -d ' \n'
}

# Write stdin to a root-owned 0600 file without exposing it in argv or logs.
install_secret() {
  local dest=$1 tmp old_umask
  old_umask=$(umask)
  umask 077
  tmp=$(mktemp)
  cat >"$tmp"
  umask "$old_umask"
  sudo install -o root -g root -m 600 "$tmp" "$dest"
  rm -f "$tmp"
}

as_owner() {
  sudo -u "$SERVICE_USER" -H env PATH="/snap/bin:/home/${SERVICE_USER}/.cargo/bin:${PATH}" "$@"
}

dns_points_here() {
  getent ahosts "$DOMAIN" 2>/dev/null | awk '$1 !~ /:/ { print $1 }' | grep -qx "$EXPECTED_A"
}

main() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --domain) DOMAIN=${2:?--domain needs a value}; shift 2 ;;
      --port) PORT=${2:?--port needs a value}; shift 2 ;;
      --branch) BRANCH=${2:?--branch needs a value}; shift 2 ;;
      --no-tls) NO_TLS=1; shift ;;
      --no-redirect) NO_REDIRECT=1; shift ;;
      -h | --help) usage; exit 0 ;;
      *) die "unknown argument: $1 (try --help)" ;;
    esac
  done

  [[ "${EUID}" -ne 0 ]] || die "do not run as root; run as a sudo-capable user."
  [[ "$DOMAIN" =~ ^[A-Za-z0-9.-]+$ ]] || die "DOMAIN must be a hostname (got ${DOMAIN})."
  [[ "$PORT" =~ ^[0-9]+$ ]] || die "PORT must be a number (got ${PORT})."
  id "$SERVICE_USER" >/dev/null 2>&1 || die "service user ${SERVICE_USER} does not exist."
  [[ -d "$APP_DIR/.git" ]] || die "${APP_DIR} is not a git checkout."
  [[ -d "$PHEXT_DIR" ]] || die "${PHEXT_DIR} does not exist."

  # sudo -v asks for a password whenever any sudoers rule needs one, even
  # with NOPASSWD: ALL alongside it, so try a no-op first.
  sudo -n true 2>/dev/null || sudo -v
  command -v nginx >/dev/null 2>&1 || [[ -x /usr/sbin/nginx ]] || die "nginx is not installed."
  sudo nginx -t || die "nginx -t fails on the existing config. Nothing was changed."

  # ── Build ──
  as_owner git -C "$APP_DIR" fetch origin "$BRANCH"
  as_owner git -C "$APP_DIR" checkout -B "$BRANCH" "origin/${BRANCH}"
  as_owner git -C "$APP_DIR" reset --hard "origin/${BRANCH}"
  local commit
  commit=$(as_owner git -C "$APP_DIR" rev-parse --short HEAD)
  printf 'Building %s\n' "$commit"
  (cd "$APP_DIR" && as_owner cargo build --release --bin phext-edit --bin phext-measure)
  [[ -x "${APP_DIR}/target/release/phext-edit" ]] || die "release binary was not produced."

  # ── Secrets ──
  sudo mkdir -p "$CONF_DIR"
  if sudo test -f "${CONF_DIR}/token.env"; then
    printf 'Keeping %s/token.env\n' "$CONF_DIR"
  else
    # Carry the token over from the old unit's --token flag, else mint one.
    local token=""
    if sudo test -f "$UNIT_PATH"; then
      token=$(sudo sed -n 's/.*--token \([^ $][^ ]*\).*/\1/p' "$UNIT_PATH" | head -1)
    fi
    [[ -n "$token" ]] || token=$(random_hex)
    printf 'PHEXT_TOKEN=%s\n' "$token" | install_secret "${CONF_DIR}/token.env"
    unset token
    printf 'Wrote %s/token.env (token not printed)\n' "$CONF_DIR"
  fi
  if sudo test -f "${CONF_DIR}/oauth.env"; then
    printf 'Keeping %s/oauth.env\n' "$CONF_DIR"
  else
    install_secret "${CONF_DIR}/oauth.env" <<EOF
# GitHub/Google sign-in for phext-edit. Leave a client line blank to keep
# that provider off. Restart after editing: sudo systemctl restart phext-edit
PHEXT_PUBLIC_URL=https://${DOMAIN}
PHEXT_SESSION_SECRET=$(random_hex)
PHEXT_COOKIE_SECURE=1
# Signing in skips the token prompt. Empty: any GitHub/Google account gets in.
# To restrict, list github:<numeric id>, google:<sub>, or email, comma-separated.
PHEXT_EDITORS=
GITHUB_CLIENT_ID=
GITHUB_CLIENT_SECRET=
GOOGLE_CLIENT_ID=
GOOGLE_CLIENT_SECRET=
EOF
    printf 'Wrote %s/oauth.env (session secret not printed). Fill in the client lines (and PHEXT_EDITORS to restrict who gets in).\n' "$CONF_DIR"
  fi

  # Funnel log. JSON lines, mode 0640, no IP addresses. See deploy/measure.md.
  sudo mkdir -p /var/lib/phext-edit
  sudo chown "$SERVICE_USER:$SERVICE_USER" /var/lib/phext-edit
  sudo chmod 750 /var/lib/phext-edit

  # ── systemd ──
  local unit_tmp
  unit_tmp=$(mktemp)
  cat >"$unit_tmp" <<EOF
[Unit]
Description=phext-edit — web-served phext editor (${DOMAIN})
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
WorkingDirectory=${APP_DIR}
EnvironmentFile=${CONF_DIR}/token.env
EnvironmentFile=-${CONF_DIR}/oauth.env
Environment=PHEXT_DATA_DIR=/var/lib/phext-edit
ExecStart=${APP_DIR}/target/release/phext-edit --dir ${PHEXT_DIR} --home ${PHEXT_HOME} --home-coord ${PHEXT_HOME_COORD} --host 127.0.0.1 --port ${PORT} --token \${PHEXT_TOKEN}
Restart=on-failure
RestartSec=5
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF
  if sudo test -f "$UNIT_PATH" && ! sudo cmp -s "$unit_tmp" "$UNIT_PATH"; then
    sudo cp -a "$UNIT_PATH" "${UNIT_PATH}.bak-$(date +%Y%m%d%H%M%S)"
  fi
  sudo install -m 644 "$unit_tmp" "$UNIT_PATH"
  rm -f "$unit_tmp"
  sudo systemctl daemon-reload
  sudo systemctl enable "$SERVICE" >/dev/null
  sudo systemctl restart "$SERVICE"

  local ok=0 attempt
  for attempt in $(seq 1 20); do
    if curl -fsS --max-time 2 -o /dev/null "http://127.0.0.1:${PORT}/"; then ok=1; break; fi
    sleep 1
  done
  if [[ "$ok" != "1" ]]; then
    sudo journalctl -u "$SERVICE" -n 30 --no-pager || true
    die "phext-edit did not answer on 127.0.0.1:${PORT}"
  fi
  sudo journalctl -u "$SERVICE" -n 8 --no-pager -o cat | grep -E 'auth:|phext-edit on' || true

  # ── nginx site ──
  local available="/etc/nginx/sites-available/${DOMAIN}"
  local enabled="/etc/nginx/sites-enabled/${DOMAIN}"
  local stamp backup="" created=0
  stamp=$(date +%Y%m%d%H%M%S)
  if sudo test -f "$available" \
    && sudo grep -qF "server_name ${DOMAIN};" "$available" \
    && sudo grep -qF "proxy_pass http://127.0.0.1:${PORT};" "$available"; then
    printf 'nginx already proxies %s to 127.0.0.1:%s\n' "$DOMAIN" "$PORT"
  else
    if sudo test -f "$available"; then
      backup="${available}.bak-${stamp}"
      sudo cp -a "$available" "$backup"
    else
      created=1
    fi
    local site_tmp
    site_tmp=$(mktemp)
    cat >"$site_tmp" <<EOF
# Managed by phext-lattice deploy/deploy.sh.
limit_req_zone \$binary_remote_addr zone=phext_edit_req:10m rate=20r/s;

server {
    listen 80;
    listen [::]:80;
    server_name ${DOMAIN};
    client_max_body_size 16M;

    location / {
        limit_req zone=phext_edit_req burst=60 nodelay;
        proxy_pass http://127.0.0.1:${PORT};
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
    }
}
EOF
    sudo install -m 644 "$site_tmp" "$available"
    rm -f "$site_tmp"
    sudo ln -sfn "$available" "$enabled"
    if ! sudo nginx -t; then
      printf 'nginx -t failed; rolling back %s\n' "$available" >&2
      if [[ "$created" == "1" ]]; then sudo rm -f "$available" "$enabled"; else sudo cp -a "$backup" "$available"; fi
      exit 1
    fi
    sudo systemctl reload nginx
    printf 'nginx now proxies %s to 127.0.0.1:%s\n' "$DOMAIN" "$PORT"
  fi

  # ── TLS ──
  local tls="no"
  if sudo test -d "/etc/letsencrypt/live/${DOMAIN}"; then
    tls="yes"
  elif [[ "$NO_TLS" == "1" ]]; then
    printf 'Skipping certbot (--no-tls).\n'
  elif ! dns_points_here; then
    printf '%s does not resolve to %s yet. Add the A record, then rerun this script.\n' "$DOMAIN" "$EXPECTED_A" >&2
  elif sudo certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos --redirect --register-unsafely-without-email; then
    tls="yes"
  else
    printf 'certbot failed; the HTTP site is still in place.\n' >&2
  fi

  # ── Old /editor on OLD_SITE → DOMAIN ──
  local old="/etc/nginx/sites-available/${OLD_SITE}"
  local redirect="skipped"
  if [[ "$NO_REDIRECT" == "1" ]]; then
    redirect="skipped (--no-redirect)"
  elif [[ "$tls" != "yes" ]]; then
    redirect="waiting for TLS on ${DOMAIN}"
  elif ! sudo test -f "$old"; then
    redirect="no ${old}"
  elif sudo grep -qF "return 301 https://${DOMAIN}/;" "$old"; then
    redirect="already in place"
    if ! sudo grep -qF 'Clear-Site-Data' "$old"; then
      # Older redirect without the cache wipe: browsers kept serving the
      # stale /editor page from cache and never saw the 301.
      local old_backup="${old}.bak-${stamp}"
      sudo cp -a "$old" "$old_backup"
      sudo sed -i "s|{ return 301 https://${DOMAIN}/; }|{ add_header Clear-Site-Data '\"cache\"' always; return 301 https://${DOMAIN}/; }|" "$old"
      if sudo nginx -t && sudo systemctl reload nginx; then
        redirect="added Clear-Site-Data to the redirect (backup ${old_backup})"
      else
        sudo cp -a "$old_backup" "$old"
        sudo systemctl reload nginx || true
        die "nginx rejected Clear-Site-Data on the ${OLD_SITE} redirect; restored ${old_backup}"
      fi
    fi
  elif sudo grep -qE 'location /editor/(api/)? \{' "$old"; then
    local old_backup="${old}.bak-${stamp}" old_tmp
    sudo cp -a "$old" "$old_backup"
    old_tmp=$(mktemp)
    # Drop the /editor/api/ proxy and swap the static /editor/ block for a
    # redirect. These blocks have no nested braces. Clear-Site-Data evicts the
    # old page, which browsers cached and would otherwise keep showing.
    sudo awk -v domain="$DOMAIN" '
      /^[[:space:]]*# Phext Editor/ { next }
      /^[[:space:]]*location \/editor\/api\/ \{/ { skip = 1; next }
      /^[[:space:]]*location \/editor\/ \{/ {
        skip = 1
        hdr = "add_header Clear-Site-Data \047\"cache\"\047 always; "
        print "    # Phext Editor moved to https://" domain "/"
        print "    location = /editor { " hdr "return 301 https://" domain "/; }"
        print "    location ^~ /editor/ { " hdr "return 301 https://" domain "/; }"
        next
      }
      skip && /^[[:space:]]*\}/ { skip = 0; next }
      skip { next }
      { print }
    ' "$old" >"$old_tmp"
    sudo install -m 644 "$old_tmp" "$old"
    rm -f "$old_tmp"
    if sudo nginx -t && sudo systemctl reload nginx; then
      redirect="${OLD_SITE}/editor → https://${DOMAIN}/ (backup ${old_backup})"
    else
      sudo cp -a "$old_backup" "$old"
      sudo systemctl reload nginx || true
      die "nginx rejected the ${OLD_SITE} redirect; restored ${old_backup}"
    fi
  else
    redirect="no /editor locations in ${old}"
  fi

  printf '\nphext-edit deploy\n'
  printf '  commit:    %s\n' "$commit"
  printf '  service:   %s (%s)\n' "$SERVICE" "$(systemctl is-active "$SERVICE")"
  printf '  listen:    127.0.0.1:%s\n' "$PORT"
  printf '  phexts:    %s\n' "$PHEXT_DIR"
  printf '  secrets:   %s/token.env, %s/oauth.env\n' "$CONF_DIR" "$CONF_DIR"
  printf '  measure:   /var/lib/phext-edit\n'
  printf '  nginx:     %s\n' "$available"
  printf '  tls:       %s\n' "$tls"
  printf '  redirect:  %s\n' "$redirect"
}

main "$@"
