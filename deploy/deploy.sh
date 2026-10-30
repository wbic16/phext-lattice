#!/usr/bin/env bash
# Deploy phext-edit at edit.phext.io with the nginx already on this host.
# Idempotent: run it again to rebuild exo, restart phext-edit, and reconcile
# the nginx site. Latches and the managed-file marker are documented in
# deploy/deploy.md.
#
#   bash deploy/deploy.sh [--check] [--no-tls] [--no-redirect]
#                         [--force-nginx] [--domain edit.phext.io]
#
# Run as a sudo-capable account (ubuntu), except --check / --dry-run, which
# do not touch the host. The checkout, build, and service belong to
# SERVICE_USER (wbic16). Secrets live in /etc/phext-edit/*.env, mode 0600,
# and are never printed.
#
# What it touches:
#   /etc/systemd/system/phext-edit.service   (backed up when it changes)
#   /etc/phext-edit/token.env, oauth.env     (created only when missing)
#   /etc/nginx/sites-{available,enabled}/DOMAIN
#       (deploy/nginx-edit.phext.io.conf once TLS exists; an HTTP bootstrap
#        before that. A hand-edited file is left in place unless
#        PHEXT_NGINX_FORCE=1 or --force-nginx.)
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
# 1 replaces a site file this script does not own. Anything else does not.
NGINX_FORCE=0
if [[ "${PHEXT_NGINX_FORCE:-0}" == "1" ]]; then
  NGINX_FORCE=1
fi
CHECK=0
# First line of site files written by older revisions of this script.
LEGACY_MARK='# Managed by phext-lattice deploy/deploy.sh.'
MANAGED_MARK='# phext-lattice-managed:'

SERVICE="phext-edit.service"
UNIT_PATH="/etc/systemd/system/${SERVICE}"
CONF_DIR="/etc/phext-edit"

die() {
  printf 'deploy.sh: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
Deploy phext-edit at edit.phext.io with the nginx already on this host.
Idempotent: run it again to rebuild exo, restart phext-edit, and reconcile
the nginx site. Latches and the managed-file marker are documented in
deploy/deploy.md.

  bash deploy/deploy.sh [--check] [--no-tls] [--no-redirect]
                        [--force-nginx] [--domain NAME] [--port N] [--branch NAME]

--check, --dry-run   Render the nginx site and self-test the file latch.
                     Does not build, restart, or write outside a temp dir.
--force-nginx        Replace a hand-edited nginx site (same as PHEXT_NGINX_FORCE=1).
                     The previous file is kept beside it as a .bak-* copy.
--no-tls             Do not run certbot.
--no-redirect        Do not change the ${OLD_SITE} /editor locations.

Run as a sudo-capable account, not root. --check does not need sudo.
EOF
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

file_sha256() {
  sha256sum "$1" | awk '{print $1}'
}

# sudo cannot change the shell redirect, so read as root and write the temp
# file as the user running this script.
read_root_file() {
  local src=$1 dest=$2
  sudo cat -- "$src" | tee "$dest" >/dev/null
}

nginx_template() {
  local candidate="${APP_DIR}/deploy/nginx-edit.phext.io.conf"
  if [[ -f "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi
  local beside
  beside="$(cd "$(dirname "$0")" && pwd)/nginx-edit.phext.io.conf"
  if [[ -f "$beside" ]]; then
    printf '%s\n' "$beside"
    return 0
  fi
  die "cannot find deploy/nginx-edit.phext.io.conf"
}

# Copy the template, fill DOMAIN and PORT, keep live certificate paths, and
# drop ssl_dhparam when its file is not on this machine.
render_site_body() {
  local template=$1 current=$2 dest=$3
  sed \
    -e "s/edit\\.phext\\.io/${DOMAIN}/g" \
    -e "s/127\\.0\\.0\\.1:8080/127.0.0.1:${PORT}/g" \
    "$template" >"$dest"
  if [[ -n "$current" && -f "$current" ]]; then
    adopt_live_ssl "$current" "$dest"
  fi
  drop_missing_dhparam "$dest"
}

adopt_live_ssl() {
  local current=$1 dest=$2 cert="" key=""
  cert=$(awk '
    /^[[:space:]]*ssl_certificate_key[[:space:]]/ { next }
    /^[[:space:]]*ssl_certificate[[:space:]]/ {
      sub(/^[[:space:]]*ssl_certificate[[:space:]]+/, "")
      sub(/[[:space:]]*;[[:space:]]*$/, "")
      print
      exit
    }
  ' "$current")
  key=$(awk '
    /^[[:space:]]*ssl_certificate_key[[:space:]]/ {
      sub(/^[[:space:]]*ssl_certificate_key[[:space:]]+/, "")
      sub(/[[:space:]]*;[[:space:]]*$/, "")
      print
      exit
    }
  ' "$current")
  [[ -n "$cert" && -n "$key" ]] || return 0
  local tmp
  tmp=$(mktemp)
  awk -v cert="$cert" -v key="$key" '
    /^[[:space:]]*ssl_certificate_key[[:space:]]/ {
      print "    ssl_certificate_key " key ";"
      next
    }
    /^[[:space:]]*ssl_certificate[[:space:]]/ {
      print "    ssl_certificate     " cert ";"
      next
    }
    { print }
  ' "$dest" >"$tmp"
  mv "$tmp" "$dest"
}

path_exists() {
  if [[ "$CHECK" == "1" ]]; then
    [[ -f "$1" ]]
  else
    sudo test -f "$1"
  fi
}

drop_missing_dhparam() {
  local dest=$1 line path tmp
  line=$(sed -n 's/^[[:space:]]*ssl_dhparam[[:space:]]\+\([^;]*\);/\1/p' "$dest" | head -n 1 || true)
  [[ -n "$line" ]] || return 0
  path=$line
  if path_exists "$path"; then
    return 0
  fi
  tmp=$(mktemp)
  grep -v '^[[:space:]]*ssl_dhparam[[:space:]]' "$dest" >"$tmp" || true
  mv "$tmp" "$dest"
}

stamp_managed() {
  local body=$1 dest=$2 extra=$3 sum
  sum=$(file_sha256 "$body")
  {
    printf '%s %s body=%s\n' "$MANAGED_MARK" "$extra" "$sum"
    cat "$body"
  } >"$dest"
}

write_bootstrap_body() {
  local dest=$1
  cat >"$dest" <<EOF
# HTTP bootstrap so certbot has a server block. deploy.sh replaces this
# with deploy/nginx-edit.phext.io.conf once /etc/letsencrypt/live/${DOMAIN}
# exists. That file is what redirects port 80 and sets HSTS.
limit_req_zone \$binary_remote_addr zone=phext_edit_req:10m rate=20r/s;

server {
    listen 80;
    listen [::]:80;
    server_name ${DOMAIN};
    server_tokens off;
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
}

# install, update, keep, or refuse. A file this script wrote (marker hash
# still matches the bytes under it, or the legacy first line) may be replaced.
# Anything else is refuse, including a managed file whose body was edited.
nginx_disposition() {
  local current=$1 rendered=$2 first="" recorded="" body_copy="" body_hash=""
  if [[ ! -f "$current" ]]; then
    printf 'install\n'
    return 0
  fi
  if cmp -s "$current" "$rendered"; then
    printf 'keep\n'
    return 0
  fi
  IFS= read -r first <"$current" || true
  if [[ "$first" == "$LEGACY_MARK" ]]; then
    printf 'update\n'
    return 0
  fi
  case "$first" in
    "${MANAGED_MARK}"*) ;;
    *)
      printf 'refuse\n'
      return 0
      ;;
  esac
  recorded=$(printf '%s\n' "$first" | sed -n 's/.* body=\([0-9a-f]\{64\}\).*/\1/p')
  body_copy=$(mktemp)
  tail -n +2 "$current" >"$body_copy"
  body_hash=$(file_sha256 "$body_copy")
  rm -f "$body_copy"
  if [[ -n "$recorded" && "$recorded" == "$body_hash" ]]; then
    printf 'update\n'
  else
    printf 'refuse\n'
  fi
}

with_force() {
  local action=$1
  if [[ "$action" == "refuse" && "$NGINX_FORCE" == "1" ]]; then
    printf 'update\n'
  else
    printf '%s\n' "$action"
  fi
}

require_hardened_body() {
  local file=$1
  grep -qF 'server_tokens off;' "$file" || die "rendered nginx site is missing server_tokens off"
  grep -qF "return 301 https://${DOMAIN}\$request_uri;" "$file" \
    || die "rendered nginx site is missing the port-80 redirect"
  grep -qF 'Strict-Transport-Security' "$file" || die "rendered nginx site is missing HSTS"
  grep -qF 'limit_req zone=phext_auth' "$file" || die "rendered nginx site is missing the auth limit_req"
  grep -qF 'limit_req zone=phext_edit_req' "$file" || die "rendered nginx site is missing the site limit_req"
  grep -qF "proxy_pass http://127.0.0.1:${PORT};" "$file" || die "rendered nginx site is missing proxy_pass"
  awk '
    /listen 80;/ { in80 = 1 }
    in80 && /proxy_pass/ { bad = 1 }
    in80 && /^}/ { in80 = 0 }
    END { exit bad ? 1 : 0 }
  ' "$file" || die "rendered port-80 server proxies instead of redirecting"
}

install_rendered_site() {
  local rendered=$1 label=$2
  local backup="" created=0
  if sudo test -f "$available"; then
    backup="${available}.bak-${stamp}"
    sudo cp -a "$available" "$backup"
    printf 'backed up nginx site to %s\n' "$backup"
  else
    created=1
  fi
  sudo install -m 644 "$rendered" "$available"
  sudo ln -sfn "$available" "$enabled"
  if ! sudo nginx -t; then
    printf 'nginx -t failed; restoring %s\n' "$available" >&2
    if [[ "$created" == "1" ]]; then
      sudo rm -f "$available" "$enabled"
    else
      sudo cp -a "$backup" "$available"
    fi
    die "nginx rejected ${label}; previous site file restored and not reloaded"
  fi
  if ! sudo systemctl reload nginx; then
    printf 'nginx reload failed; restoring %s\n' "$available" >&2
    if [[ "$created" == "1" ]]; then
      sudo rm -f "$available" "$enabled"
    else
      sudo cp -a "$backup" "$available"
    fi
    sudo systemctl reload nginx || true
    die "nginx reload failed for ${label}; previous site file restored"
  fi
  printf 'nginx reloaded (%s)\n' "$label"
}

apply_rendered_site() {
  local rendered=$1 label=$2 owned=$3
  local current action raw
  current=$(mktemp)
  if sudo test -f "$available"; then
    read_root_file "$available" "$current"
  else
    rm -f "$current"
  fi
  if [[ "$owned" == "1" ]]; then
    if [[ -f "$current" ]] && cmp -s "$current" "$rendered"; then
      action=keep
    else
      action=update
    fi
    raw=$action
  elif [[ -f "$current" ]]; then
    action=$(nginx_disposition "$current" "$rendered")
    raw=$action
    action=$(with_force "$action")
  else
    action=install
    raw=$action
  fi
  case "$action" in
    keep)
      sudo ln -sfn "$available" "$enabled"
      printf 'nginx site already current (%s)\n' "$label"
      nginx_result="current ${label}"
      ;;
    install | update)
      if [[ "$raw" == "refuse" ]]; then
        printf 'PHEXT_NGINX_FORCE: replacing %s\n' "$available"
      fi
      install_rendered_site "$rendered" "$label"
      nginx_result="reloaded ${label}"
      ;;
    refuse)
      rm -f "$current"
      die "refusing to overwrite ${available}: it has no deploy.sh marker, or the marker hash does not match the file (it was edited). Set PHEXT_NGINX_FORCE=1 or pass --force-nginx to replace it. A .bak copy is kept when that latch is set."
      ;;
    *)
      rm -f "$current"
      die "internal: bad nginx action ${action}"
      ;;
  esac
  rm -f "$current"
}

run_check() {
  local template body rendered current work
  command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"
  template=$(nginx_template)
  work=$(mktemp -d)
  body="${work}/body"
  rendered="${work}/rendered"
  current="${work}/current"
  render_site_body "$template" "" "$body"
  require_hardened_body "$body"
  local template_sum
  template_sum=$(file_sha256 "$template")
  stamp_managed "$body" "$rendered" "kind=site file=deploy/nginx-edit.phext.io.conf template=${template_sum}"
  local marked_hash body_hash
  marked_hash=$(sed -n '1s/.* body=\([0-9a-f]\{64\}\).*/\1/p' "$rendered")
  tail -n +2 "$rendered" >"${work}/tailed"
  body_hash=$(file_sha256 "${work}/tailed")
  [[ "$marked_hash" == "$body_hash" ]] || die "managed marker hash does not match the rendered body"
  [[ "$(nginx_disposition "$rendered" "$rendered")" == "keep" ]] || die "identical managed file was not kept"

  printf 'changed-body\n' >>"$body"
  stamp_managed "$body" "$current" "kind=site file=deploy/nginx-edit.phext.io.conf template=old"
  [[ "$(nginx_disposition "$current" "$rendered")" == "update" ]] || die "intact managed file was not updated"

  printf 'hand edit\n' >>"$current"
  [[ "$(nginx_disposition "$current" "$rendered")" == "refuse" ]] || die "tampered managed file was not refused"
  [[ "$(with_force "$(nginx_disposition "$current" "$rendered")")" == "refuse" ]] || die "tampered file ignored the latch default"
  NGINX_FORCE=1
  [[ "$(with_force "$(nginx_disposition "$current" "$rendered")")" == "update" ]] || die "PHEXT_NGINX_FORCE did not allow replacement"
  NGINX_FORCE=0
  if [[ "${PHEXT_NGINX_FORCE:-0}" == "1" ]]; then
    NGINX_FORCE=1
  fi

  printf 'not ours\n' >"$current"
  [[ "$(nginx_disposition "$current" "$rendered")" == "refuse" ]] || die "unmarked file was not refused"

  printf '%s\nrest of the old bootstrap\n' "$LEGACY_MARK" >"$current"
  [[ "$(nginx_disposition "$current" "$rendered")" == "update" ]] || die "legacy managed file was not updated"

  local custom="${work}/custom"
  cat >"$custom" <<EOF
    ssl_certificate     /etc/letsencrypt/live/${DOMAIN}/fullchain.pem;
    ssl_certificate_key /etc/custom/privkey.pem;
EOF
  render_site_body "$template" "$custom" "$body"
  grep -qF '/etc/custom/privkey.pem' "$body" || die "live ssl_certificate_key was not kept"
  grep -qF "live/${DOMAIN}/fullchain.pem" "$body" || die "live ssl_certificate was not kept"

  local dh="${work}/no-dh"
  printf 'ssl_dhparam %s/missing.pem;\nserver_tokens off;\n' "$work" >"$dh"
  drop_missing_dhparam "$dh"
  grep -q ssl_dhparam "$dh" && die "missing ssl_dhparam file was kept"
  printf 'ssl_dhparam %s/present.pem;\n' "$work" >"$dh"
  : >"${work}/present.pem"
  drop_missing_dhparam "$dh"
  grep -q ssl_dhparam "$dh" || die "present ssl_dhparam file was dropped"

  rm -rf "$work"
  printf 'deploy.sh --check: nginx render, marker hash, and latch ok\n'
  printf '  template:  %s\n' "$template"
  printf '  domain:    %s\n' "$DOMAIN"
  printf '  port:      %s\n' "$PORT"
  printf '  would write %s with the managed marker after a real run\n' "/etc/nginx/sites-available/${DOMAIN}"
}

main() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --domain) DOMAIN=${2:?--domain needs a value}; shift 2 ;;
      --port) PORT=${2:?--port needs a value}; shift 2 ;;
      --branch) BRANCH=${2:?--branch needs a value}; shift 2 ;;
      --no-tls) NO_TLS=1; shift ;;
      --no-redirect) NO_REDIRECT=1; shift ;;
      --force-nginx) NGINX_FORCE=1; shift ;;
      --check | --dry-run) CHECK=1; shift ;;
      -h | --help) usage; exit 0 ;;
      *) die "unknown argument: $1 (try --help)" ;;
    esac
  done

  [[ "$DOMAIN" =~ ^[A-Za-z0-9.-]+$ ]] || die "DOMAIN must be a hostname (got ${DOMAIN})."
  [[ "$PORT" =~ ^[0-9]+$ ]] || die "PORT must be a number (got ${PORT})."
  if [[ "$CHECK" == "1" ]]; then
    run_check
    return 0
  fi

  [[ "${EUID}" -ne 0 ]] || die "do not run as root; run as a sudo-capable user."
  id "$SERVICE_USER" >/dev/null 2>&1 || die "service user ${SERVICE_USER} does not exist."
  [[ -d "$APP_DIR/.git" ]] || die "${APP_DIR} is not a git checkout."
  [[ -d "$PHEXT_DIR" ]] || die "${PHEXT_DIR} does not exist."

  # sudo -v asks for a password whenever any sudoers rule needs one, even
  # with NOPASSWD: ALL alongside it, so try a no-op first.
  sudo -n true 2>/dev/null || sudo -v
  command -v nginx >/dev/null 2>&1 || [[ -x /usr/sbin/nginx ]] || die "nginx is not installed."
  command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"
  sudo nginx -t || die "nginx -t fails on the existing config. Nothing was changed."

  # ── Build ──
  as_owner git -C "$APP_DIR" fetch origin "$BRANCH"
  as_owner git -C "$APP_DIR" checkout -B "$BRANCH" "origin/${BRANCH}"
  as_owner git -C "$APP_DIR" reset --hard "origin/${BRANCH}"
  local commit
  commit=$(as_owner git -C "$APP_DIR" rev-parse --short HEAD)
  printf 'Building %s\n' "$commit"
  (cd "$APP_DIR" && as_owner cargo build --release --bin phext-edit)
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
    printf 'waiting for phext-edit (%s/20)\n' "$attempt"
    sleep 1
  done
  if [[ "$ok" != "1" ]]; then
    sudo journalctl -u "$SERVICE" -n 30 --no-pager || true
    die "phext-edit did not answer on 127.0.0.1:${PORT}"
  fi
  sudo journalctl -u "$SERVICE" -n 8 --no-pager -o cat | grep -E 'auth:|phext-edit on' || true

  # ── nginx site ──
  # phext-edit was restarted above, so the process behind the proxy is the
  # build this run just produced. nginx is reloaded only after `nginx -t`.
  local available="/etc/nginx/sites-available/${DOMAIN}"
  local enabled="/etc/nginx/sites-enabled/${DOMAIN}"
  local stamp nginx_result="unchanged" tls="no" owned=0 skip_certbot=0
  local template body rendered current_copy first_line
  stamp=$(date +%Y%m%d%H%M%S)
  template=$(nginx_template)
  body=$(mktemp)
  rendered=$(mktemp)
  current_copy=$(mktemp)
  if sudo test -f "$available"; then
    read_root_file "$available" "$current_copy"
  else
    rm -f "$current_copy"
  fi

  if sudo test -d "/etc/letsencrypt/live/${DOMAIN}"; then
    tls="yes"
  fi

  # Certbot's nginx plugin needs a server block. Write the HTTP bootstrap
  # only when this script already owns the file (or there is no file yet).
  # A hand-edited site is not replaced here.
  if [[ "$tls" != "yes" ]]; then
    local bootstrap
    bootstrap=$(mktemp)
    write_bootstrap_body "$body"
    stamp_managed "$body" "$bootstrap" "kind=http-bootstrap"
    if [[ -f "$current_copy" ]]; then
      first_line=""
      IFS= read -r first_line <"$current_copy" || true
    else
      first_line=""
    fi
    if [[ "$first_line" == *"kind=site"* && "$first_line" == *"file=deploy/nginx-edit.phext.io.conf"* ]]; then
      printf 'TLS directory for %s is missing; leaving the hardened nginx site in place.\n' "$DOMAIN"
      nginx_result="left hardened site (no TLS directory)"
      skip_certbot=1
    elif [[ "$first_line" == "$LEGACY_MARK" ]]; then
      owned=1
      printf 'existing nginx site is the previous deploy.sh bootstrap; certbot may edit it\n'
    else
      apply_rendered_site "$bootstrap" "http bootstrap" 0
      owned=1
    fi
    rm -f "$bootstrap"
  fi

  if [[ "$skip_certbot" == "1" ]]; then
    printf 'Skipping certbot; the hardened site file is already installed.\n'
  elif [[ "$tls" != "yes" && "$NO_TLS" == "1" ]]; then
    printf 'Skipping certbot (--no-tls).\n'
  elif [[ "$tls" != "yes" ]] && ! dns_points_here; then
    printf '%s does not resolve to %s yet. Add the A record, then rerun this script.\n' "$DOMAIN" "$EXPECTED_A" >&2
  elif [[ "$tls" != "yes" ]]; then
    if sudo certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos --redirect --register-unsafely-without-email; then
      tls="yes"
    else
      printf 'certbot failed; the HTTP site is still in place.\n' >&2
    fi
  fi

  if sudo test -d "/etc/letsencrypt/live/${DOMAIN}"; then
    tls="yes"
  fi

  if [[ "$tls" == "yes" ]]; then
    if sudo test -f "$available"; then
      read_root_file "$available" "$current_copy"
    else
      rm -f "$current_copy"
    fi
    render_site_body "$template" "$current_copy" "$body"
    require_hardened_body "$body"
    local template_sum
    template_sum=$(file_sha256 "$template")
    stamp_managed "$body" "$rendered" "kind=site file=deploy/nginx-edit.phext.io.conf template=${template_sum}"
    # `owned` means the bytes on disk started this run as our bootstrap (or
    # we just wrote them). certbot may have edited them; still replace with
    # the hardened site. A file we did not own goes through the latch.
    apply_rendered_site "$rendered" "deploy/nginx-edit.phext.io.conf" "$owned"
  fi
  rm -f "$body" "$rendered" "$current_copy"

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
    ' "$old" | tee "$old_tmp" >/dev/null
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
  printf '  nginx:     %s (%s)\n' "$available" "$nginx_result"
  printf '  tls:       %s\n' "$tls"
  printf '  redirect:  %s\n' "$redirect"
}

main "$@"
