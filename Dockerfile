# phext-edit — container image for edit.phext.io.
#
# Two stages: build the release binary with the Rust toolchain, then run
# it on slim Debian with only the binary, its entrypoint, CA roots, and
# curl (the healthcheck's client). Every web asset is compiled into the
# binary (include_str! in src/bin/phext_edit.rs), so nothing else ships.
#
# Configuration is environment-only. No secret is baked into this image:
# sign-in settings arrive via the container environment at run time —
# see deploy/CONTAINER.md for the full contract and
# deploy/phext-edit.env.example for the variable list.

# Base images are pinned to a release line (never a floating `latest`).
# The digests these tags resolved to when this file was verified are
# recorded in deploy/CONTAINER.md.
FROM rust:1.94.0-bookworm AS build

WORKDIR /src
COPY . .

# The same build deploy/deploy.sh runs on the host.
RUN cargo build --release --bin phext-edit \
    && strip target/release/phext-edit

FROM debian:bookworm-slim

# ca-certificates: the OAuth token exchanges are outbound HTTPS.
# curl: the HEALTHCHECK below probes /api/session with it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --no-create-home phext \
    && mkdir -p /data \
    && chown phext:phext /data

COPY --from=build /src/target/release/phext-edit /usr/local/bin/phext-edit
COPY deploy/container-entrypoint.sh /usr/local/bin/container-entrypoint.sh

# The phext library lives here, on a volume (see compose.yaml). The
# entrypoint serves --dir /data unless PHEXT_DATA_DIR says otherwise.
VOLUME ["/data"]
WORKDIR /data

USER phext
EXPOSE 8080

# /api/session answers without a session in both gate shapes (locked:
# the providers list; open: the session body), so it is a safe liveness
# probe that never needs a credential.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${PHEXT_EDIT_PORT:-8080}/api/session" >/dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/container-entrypoint.sh"]
