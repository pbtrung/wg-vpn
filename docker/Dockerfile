# Production image for running `wg-server apply` as a scheduled cloud
# cron job (docs/wg-server.md §1: "an ephemeral cloud cron job... no
# persistent filesystem"). Build from the repo root:
#
#   docker build -f docker/Dockerfile -t wg-server .
#
# The topology config path is read from the WG_SERVER_CONFIG
# environment variable (see docker/README.md) rather than baked in, so
# the same image works across environments/fleets.

FROM rust:1.98-slim-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --locked -p wg-server

# distroless: no shell, no package manager, minimal attack surface.
# wg-server only makes outbound HTTPS calls to the storage backend and
# never touches a live network interface (that's wg-client's job), so it
# needs nothing beyond glibc/libgcc — the "cc" variant provides exactly
# that. Runs as the image's built-in non-root user.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /build/target/release/wg-server /usr/local/bin/wg-server

ENTRYPOINT ["/usr/local/bin/wg-server", "apply"]
