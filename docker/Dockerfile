# Production image for running `wg-server apply` as a scheduled cloud
# cron job (docs/wg-server.md §1: "an ephemeral cloud cron job... no
# persistent filesystem"). The builder clones its own source from git
# instead of copying the local build context, so this can build from
# anywhere (no repo checkout required) and always builds a known,
# committed ref rather than whatever uncommitted state happens to be on
# disk:
#
#   docker build -f docker/Dockerfile -t wg-server .
#   docker build -f docker/Dockerfile -t wg-server --build-arg WG_VPN_REF=v1.2.3 .
#
# The topology config path is read from the WG_SERVER_CONFIG
# environment variable (see docker/README.md) rather than baked in, so
# the same image works across environments/fleets.
#
# Alpine edge for both stages (not just numbered/stable releases) so
# `apk upgrade` actually pulls the latest available package builds, and
# so the builder's musl-linked binary matches the runtime's musl libc —
# a glibc-built binary (e.g. from a Debian-based builder) will not run
# on Alpine.

FROM alpine:edge AS builder
RUN apk update && apk --no-cache upgrade
RUN apk add --no-cache rust cargo musl-dev gcc cmake make perl git
ARG WG_VPN_REPO=https://github.com/pbtrung/wg-vpn.git
ARG WG_VPN_REF=main
WORKDIR /build
RUN git clone --depth 1 --branch "$WG_VPN_REF" "$WG_VPN_REPO" .
RUN cargo build --release --locked -p wg-server \
    && strip target/release/wg-server

FROM alpine:edge
RUN apk update && apk --no-cache upgrade
# libgcc: the release binary links against it for unwinding (panics),
# even though nothing else here needs a compiler toolchain.
RUN apk add --no-cache ca-certificates libgcc \
    && addgroup -S wg-server \
    && adduser -S -G wg-server -H -D wg-server

COPY --from=builder /build/target/release/wg-server /usr/local/bin/wg-server

USER wg-server
# --rotate (bare = every node) + -v: this image force-rotates every
# node's key on every scheduled run (see docker/README.md's "Rotation"
# note for why that's an intentional deviation from the general
# recommended cron usage in docs/wg-server.md).
ENTRYPOINT ["/usr/local/bin/wg-server", "apply", "--rotate", "-v"]
