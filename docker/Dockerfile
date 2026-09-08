# Production image for running `wg-server apply` as a scheduled cloud
# cron job (docs/wg-server.md §1: "an ephemeral cloud cron job... no
# persistent filesystem"). Build from the repo root:
#
#   docker build -f docker/Dockerfile -t wg-server .
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
RUN apk add --no-cache rust cargo musl-dev gcc cmake make perl
WORKDIR /build
COPY . .
RUN cargo build --release --locked -p wg-server

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
