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
# GitHub links a ghcr package to a repository by this label. Without it
# the package attaches to whichever repo it was first connected to by
# hand, which is how it can end up filed under an unrelated project.
LABEL org.opencontainers.image.source="https://github.com/bisand/kubeloupe"
LABEL org.opencontainers.image.description="Metrics for Lens Desktop in one static binary: reads the Kubernetes API and kubelet directly, serves the PromQL subset Lens generates."
LABEL org.opencontainers.image.licenses="Apache-2.0"
# `strip` is left out on purpose, matching postbud: a panic in production
# still names functions in the backtrace, for about 1 MB.
COPY --from=builder /src/target/release/kubeloupe /kubeloupe
# No CA bundle: the only TLS peer is the API server, whose CA is mounted
# from the ServiceAccount at runtime.
ENTRYPOINT ["/kubeloupe"]
