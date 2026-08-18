# rust:alpine targets musl natively, so the binary is static and the
# runtime image can be scratch: no base OS, nothing to patch, nothing to
# exploit. The built image is around 4 MB.
#
# The builder is deliberately NOT pinned to $BUILDPLATFORM: it must run as
# the TARGET platform so the binary matches the node. Building on an
# arm64 laptop for an amd64 node therefore goes through emulation and
# takes a few minutes -- which is the correct trade against shipping a
# binary the node cannot execute.
FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM scratch
# `strip` is left out on purpose, matching postbud: a panic in production
# still names functions in the backtrace, for about 1 MB.
COPY --from=builder /src/target/release/lens-metricsd /lens-metricsd
# No CA bundle: the only TLS peer is the API server, whose CA is mounted
# from the ServiceAccount at runtime.
ENTRYPOINT ["/lens-metricsd"]
