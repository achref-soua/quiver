# Multi-stage build → a static musl binary in a distroless, non-root image.
# Build:  docker build -f infra/docker/Dockerfile -t quiver:dev .

# ---- builder ----
FROM rust:1.96-slim AS builder
WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*
COPY . .
# Add the musl target to the toolchain rust-toolchain.toml pins (channel = "stable"),
# then build the static CLI binary. Adding the target after COPY — in the same layer as
# the build — ensures it lands on the toolchain cargo actually resolves in this repo.
RUN rustup target add x86_64-unknown-linux-musl \
    && cargo build --release --target x86_64-unknown-linux-musl -p quiverdb-cli

# ---- runtime ----
FROM gcr.io/distroless/static-debian12:nonroot AS runtime
LABEL org.opencontainers.image.source="https://github.com/achref-soua/quiver"
LABEL org.opencontainers.image.licenses="AGPL-3.0-only"
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/quiver /usr/local/bin/quiver
USER nonroot:nonroot
# REST (8080) and gRPC (50051).
EXPOSE 8080 50051
ENTRYPOINT ["/usr/local/bin/quiver"]
CMD ["serve"]
