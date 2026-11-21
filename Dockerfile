# Shopping Sessions: one Rust binary with no crate dependencies.
# The scorers build this natively on linux/amd64; on an Apple-silicon Mac the
# builder stage cross-compiles, so `docker buildx build --platform linux/amd64`
# stays fast there too.
FROM --platform=$BUILDPLATFORM rust:1.85-slim-bookworm AS toolchain-arm64
# The 512-bit intrinsics are stable on Rust 1.90.
FROM --platform=$BUILDPLATFORM rust:1.90-slim-bookworm AS toolchain-amd64
FROM toolchain-${TARGETARCH} AS builder
ARG BUILDARCH
ARG TARGETARCH
ARG AMD64_CPU=x86-64-v4
ARG PGO_TRAIN=auto
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
COPY pgo-generator/ /pgo-generator/
# Default to the scorer ISA. Override AMD64_CPU for emulator compatibility.
RUN target="$(head -n 1 /target-info)" && linker="$(tail -n 1 /target-info)" && \
    if [ "$BUILDARCH" = "$TARGETARCH" ]; then linker=cc; fi && \
    case "$TARGETARCH" in amd64) cpu="$AMD64_CPU" ;; arm64) cpu=generic ;; esac && \
    features= && \
    if [ "$cpu" = "x86-64-v4" ]; then features="-C target-feature=+avx512f,+avx512bw,+avx512vl"; fi && \
    flags="-C linker=$linker -C target-cpu=$cpu $features" && \
    train=0 && \
    if [ "$BUILDARCH" = "$TARGETARCH" ]; then \
        if [ "$PGO_TRAIN" = 1 ] || { [ "$PGO_TRAIN" = auto ] && [ "$cpu" = x86-64-v4 ] && grep -qw avx512bw /proc/cpuinfo; }; then train=1; fi; \
    fi && \
    if [ "$train" = 1 ]; then \
        rustup component add llvm-tools-preview && \
        profdata="$(find "$(rustc --print sysroot)" -name llvm-profdata -type f -print -quit)" && \
        test -x "$profdata" && mkdir -p /tmp/pgo /tmp/train-out && \
        cargo run --manifest-path /pgo-generator/Cargo.toml --locked --release -- \
            --seed 1 --events 1000000 --clicks 250000 --output /tmp/train-data && \
        RUSTFLAGS="$flags -C profile-generate=/tmp/pgo" cargo build --locked --release --target "$target" && \
        LLVM_PROFILE_FILE=/tmp/pgo/%m-%p.profraw DATA_DIR=/tmp/train-data OUTPUT_DIR=/tmp/train-out NCPU=8 \
            "target/$target/release/shopping-sessions" && \
        "$profdata" merge -o /tmp/pgo/merged.profdata /tmp/pgo/*.profraw && \
        flags="$flags -C profile-use=/tmp/pgo/merged.profdata"; \
    fi && \
    RUSTFLAGS="$flags" cargo build --locked --release --target "$target" && \
    cp "target/$target/release/shopping-sessions" /shopping-sessions

FROM debian:bookworm-slim
COPY --from=builder /shopping-sessions /shopping-sessions
ENTRYPOINT ["/shopping-sessions"]
