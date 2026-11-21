# Shopping Sessions: one Rust binary with no crate dependencies.
# The scorers build this natively on linux/amd64; on an Apple-silicon Mac the
# builder stage cross-compiles, so `docker buildx build --platform linux/amd64`
# stays fast there too.
FROM --platform=$BUILDPLATFORM rust:1.85-slim-bookworm AS builder
ARG BUILDARCH
ARG TARGETARCH
WORKDIR /src
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu; linker=x86_64-linux-gnu-gcc; cross=gcc-x86-64-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu; linker=aarch64-linux-gnu-gcc; cross=gcc-aarch64-linux-gnu ;; \
      *) exit 1 ;; \
    esac && \
    if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
        apt-get update && apt-get install -y --no-install-recommends "$cross" "libc6-dev-$TARGETARCH-cross"; \
    fi && \
    rustup target add "$target" && printf '%s\n%s\n' "$target" "$linker" > /target-info
COPY native/ ./
# x86-64-v3 (AVX2, BMI2): every scorer machine is Cascade Lake or newer.
RUN target="$(head -n 1 /target-info)" && linker="$(tail -n 1 /target-info)" && \
    if [ "$BUILDARCH" = "$TARGETARCH" ]; then linker=cc; fi && \
    case "$TARGETARCH" in amd64) cpu=x86-64-v3 ;; arm64) cpu=generic ;; esac && \
    RUSTFLAGS="-C linker=$linker -C target-cpu=$cpu" \
      cargo build --locked --release --target "$target" && \
    cp "target/$target/release/shopping-sessions" /shopping-sessions

FROM debian:bookworm-slim
COPY --from=builder /shopping-sessions /app/shopping-sessions
CMD ["/app/shopping-sessions"]
