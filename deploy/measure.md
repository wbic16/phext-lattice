# Ad measurement (phext-edit)

A first-party funnel for ad flights. The editor page records five steps and
posts them to `POST /api/measure` on the same origin. The server appends JSON
lines to `$PHEXT_DATA_DIR/measure.jsonl` and rotates that file. `phext-measure`
prints the funnel.

No third-party script is loaded. The log has no IP address, no cookie, and no
account id. `gclid`, `fbclid`, and `twclid` are stored as booleans. A request
with `DNT: 1` or `Sec-GPC: 1` is discarded.

## What Will does on the server

`deploy/deploy.sh` creates `/var/lib/phext-edit`, points the service at it, and
builds `phext-measure`. Re-run the script, then confirm the startup line:

```bash
sudo journalctl -u phext-edit -n 30 --no-pager -o cat | grep measure
# measure: /var/lib/phext-edit/measure.jsonl
```

To turn it on without a full deploy:

```bash
sudo mkdir -p /var/lib/phext-edit
sudo chown wbic16:wbic16 /var/lib/phext-edit
sudo chmod 750 /var/lib/phext-edit
sudo install -d -o root -g root -m 755 /etc/systemd/system/phext-edit.service.d
sudo tee /etc/systemd/system/phext-edit.service.d/measure.conf >/dev/null <<'EOF'
[Service]
Environment=PHEXT_DATA_DIR=/var/lib/phext-edit
EOF
sudo systemctl daemon-reload
sudo systemctl restart phext-edit
```

Build the report tool if this checkout was not deployed with the script above:

```bash
cd /source/phext-lattice
sudo -u wbic16 cargo build --release --bin phext-measure
```

Read the funnel after a flight (the log is mode 0640, owned by the service user):

```bash
sudo -u wbic16 /source/phext-lattice/target/release/phext-measure /var/lib/phext-edit
```

Counts are unique browser tabs (`sid` in `sessionStorage`), grouped by
`utm_campaign` and `utm_source` from the earliest note in that tab.

## Event names

`window.track(name, extra)` is defined in the editor page (the block marked
`MEASUREMENT`). It does nothing when Do Not Track or Global Privacy Control
is visible to the script. Names:

| `name` | when it fires |
| --- | --- |
| `landing` | once per tab, when the editor page loads |
| `demo_start` | call this when a signed-out demo is shown |
| `signin_click` | a sign-in button; `extra.provider` is `github`, `google`, or `token` |
| `signin_done` | sign-in finished; same `provider` |
| `file_first` | once per tab, the first phext opened or created |

```javascript
track('demo_start');
track('signin_click', { provider: 'github' });
```

A separate HTML document (a demo page that is not the editor) needs the
`MEASUREMENT` script from the editor copied into it before `track` exists.
Attribution is captured from the first call in that tab and kept in
`sessionStorage`, so it survives the sign-in redirect. Click ids are recorded
as booleans only.

`POST /api/measure` is accepted without a sign-in cookie. It is registered in
`handle_request` before the auth gate.

## CSP

This change does not set a Content-Security-Policy. The beacon is a same-origin
POST to `/api/measure`. `connect-src 'self'` allows it. No other host is
required. The helper is an inline script in the editor page, so a `script-src`
that blocks inline scripts would also block the beacon.

## Limits

- Body cap 2048 bytes. Unknown JSON keys are refused, so a click-id string
  cannot be written.
- 600 notes per minute for the process, 30 per session id, counted in memory
  for 60 seconds. The limiter does not store IP addresses.
- The live file rotates at 1 MiB. `measure.jsonl.1` through `.3` are kept
  (about 4 MiB). `phext-measure` reads all of them.
- A cross-origin `Origin` (host different from `Host`) is refused and not logged.

## Access log (optional)

The measurement file stores `gclid` / `fbclid` / `twclid` as yes or no. Nginx's
default access log still records the request URI, query string included, so the
landing URL's raw click id can appear there. The app does not change nginx.
To drop query strings from the access log for this site:

```nginx
log_format phext_edit '$remote_addr - $remote_user [$time_local] '
                      '"$request_method $uri $server_protocol" $status $body_bytes_sent '
                      '"$http_referer" "$http_user_agent"';
access_log /var/log/nginx/edit.phext.io.access.log phext_edit;
```

`$uri` is the path without the query string. `$http_referer` can still contain
a query; remove that field too if the referrer URL should not be kept.
