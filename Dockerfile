# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY native/ ./
RUN rustup target add x86_64-unknown-linux-musl \
 && RUSTFLAGS="-C target-cpu=x86-64-v4 -C linker=musl-gcc" \
    cargo build --locked --release --target x86_64-unknown-linux-musl \
 && cp target/x86_64-unknown-linux-musl/release/shopping-sessions /shopping-sessions

FROM scratch
COPY --from=builder /shopping-sessions /app/native
ENTRYPOINT ["/app/native"]
