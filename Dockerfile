# JavaScript build stage (webpack outputs to ../static/build relative to app/)
FROM node:lts-slim AS js-builder

WORKDIR /build/app

COPY app/package*.json ./
RUN npm ci

COPY app/ ./
RUN mkdir -p /build/static && npm run build

# Rust build stage
FROM rust:1-bookworm AS rust-builder

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates

# the version is compiled into the binary (shown in the UI title and with --version)
ARG VERSION=dev
ENV KUBE_OPS_VIEW_VERSION=${VERSION}

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked && \
    cp target/release/kube-ops-view /usr/local/bin/kube-ops-view

# Runtime stage: glibc + CA certificates only, no shell, no package manager
FROM gcr.io/distroless/cc-debian12:nonroot

WORKDIR /app

COPY --from=rust-builder /usr/local/bin/kube-ops-view /app/kube-ops-view
COPY static/favicon.ico static/sharetechmono.woff2 ./static/
COPY --from=js-builder /build/static/build ./static/build

# same UID as in deploy/deployment.yaml (runAsUser: 1001)
USER 1001

ENV STATIC_DIR=/app/static
EXPOSE 8080

ENTRYPOINT ["/app/kube-ops-view"]
