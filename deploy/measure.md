# Ad measurement (phext-edit)

A first-party funnel for ad flights. The editor page records five steps and
posts them to `POST /api/measure` on the same origin. The server appends JSON
lines to `$PHEXT_DATA_DIR/measure.jsonl` and rotates that file. `phext-measure`
prints the funnel.

No third-party script is loaded. The log has no IP address, no cookie, and no
account id. `gclid`, `fbclid`, and `twclid` are stored as booleans. A request
with `DNT: 1` or `Sec-GPC: 1` is discarded.

## What Will does on the server

`deploy/deploy.sh` is the install. It creates `/var/lib/phext-edit`, sets
`PHEXT_DATA_DIR` on the phext-edit unit, and builds `phext-measure`. Re-run
the script, then confirm the startup line:

```bash
sudo journalctl -u phext-edit -n 30 --no-pager -o cat | grep measure
# measure: /var/lib/phext-edit/measure.jsonl
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
| `signin_click` | a sign-in button; `extra.provider` is `github`, `google`, `x`, or `token` |
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

## Access log

The measurement file stores `gclid` / `fbclid` / `twclid` as yes or no. Nginx's
access log can still record the request URI, query string included, so the
landing URL's raw click id can appear there. This deploy does not change nginx.
Any access-log change belongs in `deploy/deploy.sh` and `deploy/nginx-edit.phext.io.conf`,
not in a hand edit on the host.
