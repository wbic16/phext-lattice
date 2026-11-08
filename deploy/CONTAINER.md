# Running phext-edit in a container

An additional deployment path for the edit.phext.io server: the same
`phext-edit` binary, the same environment contract, and the same data
directory as the host deployment — packaged as an image and run under
Docker Compose. **The host path stays.** `deploy/deploy.sh`, the
systemd unit, and `/etc/phext-edit/oauth.env` remain the supported way
to run the current production host; nothing in this document replaces
them. Pick one path per host, and never run both against the same
port or the same phext data.

Files added by this path:

| File | Role |
| --- | --- |
| `Dockerfile` | Multi-stage build: Rust toolchain stage, slim Debian runtime, non-root user |
| `.dockerignore` | Keeps the build context (and image) free of VCS state, build output, and filled-in env files |
| `compose.yaml` | The `edit` service, its data volume, env wiring, and hardening |
| `deploy/container-entrypoint.sh` | Maps the wiring variables below onto `phext-edit` flags |
| `deploy/phext-edit.env.example` | The sign-in environment, placeholders only |

The Dockerfile sits at the repository root because the build context
is the repository: the binary compiles `web/` assets in with
`include_str!`, so the runtime image needs nothing but the binary
itself, its entrypoint, CA roots (outbound OAuth token exchanges are
HTTPS), and `curl` for the healthcheck.

Hardening, and one deliberate omission: the compose service runs
with a read-only filesystem and all capabilities dropped, mirroring
the systemd unit's posture — except `NoNewPrivileges`. Under
Docker's equivalent (`security_opt: no-new-privileges`) the
AppArmor-confined snap Docker used to verify this path refused
every `exec`, the server binary included, and the container could
not start at all. The flag is therefore left out of `compose.yaml`
with the reason recorded there; on a container runtime that
supports it, adding it back restores the unit's last hardening.

## Prerequisites

- A host with Docker Engine and the Compose plugin (`docker compose
  version` answers). Building pulls the pinned base images
  (`rust:1.94.0-bookworm`, `debian:bookworm-slim`) and crates from
  crates.io, so the build host needs registry and crates.io access.
- For the production shape: the host's nginx, configured by
  `deploy/deploy.sh` from `deploy/nginx-edit.phext.io.conf`, in front
  (see *Reverse proxy* below).

## The environment contract

Sign-in configuration is identical to the host deployment — the same
variables `deploy.sh` writes to `/etc/phext-edit/oauth.env`. For the
container they live in `phext-edit.env` in the repository root, which
`compose.yaml` passes through `env_file`. **Values are injected at run
time only.** No credential is committed, copied into the image, or
accepted as a build argument; `.dockerignore` keeps the filled env
file out of the build context as well.

| Variable | Secret | Meaning |
| --- | --- | --- |
| `PHEXT_PUBLIC_URL` | no | Public origin, e.g. `https://edit.phext.io`. OAuth callbacks are `<origin>/api/auth/<provider>/callback`. |
| `PHEXT_SESSION_SECRET` | **yes** | Session signing key. Generate once: `openssl rand -hex 32`. |
| `PHEXT_COOKIE_SECURE` | no | `1` when the public URL is https. |
| `PHEXT_EDITORS` | no | Editor allowlist: `github:<numeric id>`, `google:<sub>`, `x:<id>`, or verified emails, comma-separated. Empty = any signed-in account. |
| `PHEXT_EDITORS_FILE` | no | Optional path for the live editors file. Unset: `<data dir>/admitted-editors.txt`, on the volume. |
| `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET` | **yes** (secret) | GitHub OAuth app. Empty pair = provider off. |
| `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` | **yes** (secret) | Google OAuth client. Empty pair = provider off. |
| `X_CLIENT_ID` / `X_CLIENT_SECRET` | **yes** (secret) | X OAuth app. Empty pair = provider off. |
| `PHEXT_CLAIM_ENABLED` | no | `1` opens claim mode (`/api/claim`). |
| `PHEXT_SSO_SECRET` | **yes** | Shared secret for the SSO handoffs. A dedicated key — never reuse `PHEXT_SESSION_SECRET`. See `deploy/deploy.md` for the deploy order. |
| `PHEXT_SSO_MUTABLE_URL` | no | Mutable origin for the SSO handoff (default `https://mutable.phext.io` when the secret is set). |
| `PHEXT_SSO_EXOCORTEXIA_URL` | no | Exocortexia origin. Unset = that handoff answers 404. |
| `PHEXT_SSO_WISHNODE_URL` | no | Wishnode origin. Unset = that handoff answers 404. |
| `PHEXT_SSO_DESKTOP` | no | Arming flag for the desktop (Phext Notepad) SSO audience: `1` arms it. Not a secret, and there is no desktop origin to configure — the return is the app's per-attempt loopback listener. Unset = both desktop endpoints answer 404. |
| `PHEXT_SPONSOR_URL` | no | HTTPS Stripe Payment Link for the Support the site donation. `deploy.sh` writes `https://donate.stripe.com/9B6dRa2t08tu7jUgxo5Vu0b` when unset. Empty hides the link. Not an ad, and not a Stripe secret. |

With no provider configured and no session secret, the gate is open —
anyone who can reach the port can edit. That shape is for local
evaluation only; a public container always sets the session secret and
at least one provider pair, which is also what makes the gate report
`locked: true`.

Container wiring variables (read by `deploy/container-entrypoint.sh`,
not by the server). Their defaults reproduce the production invocation
from the systemd unit:

| Variable | Default | Becomes |
| --- | --- | --- |
| `PHEXT_DATA_DIR` | `/data` | `--dir` (the phext library; the volume mount point) |
| `PHEXT_HOME` | `choose-your-own-adventure.phext` | `--home` |
| `PHEXT_HOME_COORD` | `10.10.2/1.1.1/1.10.10` | `--home-coord` |
| `PHEXT_EDIT_HOST` | `0.0.0.0` | `--host` (inside the container; see *Reverse proxy*) |
| `PHEXT_EDIT_PORT` | `8080` | `--port` |

## Build

```bash
docker compose build
# or, without compose:
docker build -t phext-edit:local .
```

The build stage runs the same command as `deploy.sh`
(`cargo build --release --bin phext-edit`). The repository does not
pin a `Cargo.lock` (it is gitignored), so dependency resolution
follows the version ranges in `Cargo.toml`, as on the host path.

Base-image tags in the Dockerfile, and the digests they resolved to
when this path was verified (2026-11-08). The tags are the contract;
the digests are the verification record — if a rebuild resolves a
different digest, the base image moved within its release line:

| Tag | Verified digest |
| --- | --- |
| `rust:1.94.0-bookworm` | `sha256:365468470075493dc4583f47387001854321c5a8583ea9604b297e67f01c5a4f` |
| `debian:bookworm-slim` | `sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc` |

## First run

The server serves a directory of phexts and exits if it finds none,
so seed the volume with the library before the first start. The
library is the same directory the host path serves (`/source/human`
in production):

```bash
cp deploy/phext-edit.env.example phext-edit.env   # then fill it in
chmod 600 phext-edit.env

# Seed the named volume from a local checkout of the library. The
# server runs as uid 10001, so hand it ownership of the seeded files.
docker run --rm -v phext-data:/data -v /source/human:/seed:ro \
    debian:bookworm-slim \
    sh -c 'cp -a /seed/. /data/ && chown -R 10001:10001 /data'
# The home phext must be among the seeded files:
docker run --rm -v phext-data:/data debian:bookworm-slim \
    ls /data/choose-your-own-adventure.phext

docker compose up -d
docker compose ps          # status should become "healthy"
curl -s http://127.0.0.1:8080/api/session
# {"locked":true,"providers":["github","google", ...]} with sign-in set,
# or the open-session body when it is not.
```

Health: the image's `HEALTHCHECK` probes `/api/session`, which
answers without a credential in both gate shapes. `docker inspect`
shows the current health status; an unhealthy container after a config
change usually means the server exited — `docker compose logs edit`
says why (a missing home phext and a bad coordinate both exit loudly).

## Reverse proxy

The container does not terminate TLS and ships no proxy. Compose
publishes the port on **host loopback only** (`127.0.0.1:8080`), so
the host's existing nginx site — `deploy/nginx-edit.phext.io.conf`,
installed and reconciled by `deploy.sh` — proxies to it unchanged:
same `proxy_pass http://127.0.0.1:8080`, same rate-limit zones, same
headers, same HSTS handling. The server binds `0.0.0.0` *inside* the
container so the published port can reach it; the loopback-only
publish is what keeps it off the network, mirroring the systemd
unit's `127.0.0.1` bind.

One line of that contract is load-bearing and easy to lose in a
hand-written proxy: **`proxy_http_version 1.1` must stay.** The
shipped site sets it in every location. The standing rule for
proxying anywhere in this stack is HTTP/1.1 — SQ, the phext serving
layer behind the same proxy family, answers an HTTP/1.0 upstream
with `403 Forbidden: invalid phext path`, and a proxy rewrite that
drops the line tends to surface as exactly that failure.

## Data and backups

Everything the server writes lives in the `phext-data` volume at
`/data`: the phext files themselves, `admitted-editors.txt`, and the
`claims/`, `joins/`, and `submissions/` subdirectories. The
container's filesystem is read-only outside `/tmp`; an image rebuild
never touches the volume.

Back the volume up like any data directory:

```bash
docker run --rm -v phext-data:/data -v "$PWD":/backup \
    debian:bookworm-slim \
    tar czf /backup/phext-data-$(date +%Y%m%d).tar.gz -C /data .
```

Restore by untarring into a fresh volume with the stack stopped.
Note the server memory-maps the open phext: take backups at a quiet
moment, or stop the service (`docker compose stop edit`) for a
crisp copy.

## Update and rollback

```bash
git pull            # or fetch the branch you deploy
docker compose build
docker compose up -d
```

Compose recreates the container on the new image; the volume carries
over. To roll back, check out the previous commit and repeat — or,
if you tagged the previous image (`docker tag phext-edit:local
phext-edit:<commit>` before updating, which is cheap insurance),
point `image:` at that tag and `docker compose up -d` without
building. Configuration rollbacks are the same shape: restore the
previous `phext-edit.env` and recreate the container. There is no
database migration in either direction; the phext files are the
state.

## Relationship to deploy.sh

`deploy.sh` deploys the binary directly on the host under systemd and
reconciles the nginx site; this path deploys the same binary in a
container and leaves nginx where `deploy.sh` put it. The two share
the environment contract, the data directory layout, the port, and
the proxy contract, so moving a host between paths is: stop one,
seed or reuse the same data, start the other. Do not run both at
once against the same data directory — two processes memory-mapping
the same phext file will corrupt it.
