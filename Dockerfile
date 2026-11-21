FROM --platform=$BUILDPLATFORM rust:1.85-slim-bookworm AS builder
ARG BUILDARCH
ARG TARGETARCH
WORKDIR /src

RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl; linker=musl-gcc; cross=musl-tools ;; \
      arm64) target=aarch64-unknown-linux-musl; linker=aarch64-linux-musl-gcc; cross=gcc-aarch64-linux-gnu ;; \
      *) exit 1 ;; \
    esac && \
    if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
      apt-get update && apt-get install -y --no-install-recommends "$cross"; \
    fi && \
    rustup target add "$target" && \
    printf '%s\n%s\n' "$target" "$linker" > /target-info

COPY native/ ./
RUN target="$(head -n 1 /target-info)" && linker="$(tail -n 1 /target-info)" && \
    if [ "$BUILDARCH" = "$TARGETARCH" ]; then linker=cc; fi && \
    case "$TARGETARCH" in
      amd64) cpu=x86-64-v4 ;;   # AVX-512: Cascade Lake or newer
      arm64) cpu=generic ;;
    esac && \
    RUSTFLAGS="-C linker=$linker -C target-cpu=$cpu -C link-arg=-static" \
      cargo build --locked --release --target "$target" && \
    cp "target/$target/release/shopping-sessions" /shopping-sessions

FROM scratch
COPY --from=builder /shopping-sessions /app/native
ENTRYPOINT ["/app/native"]
