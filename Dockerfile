FROM rust:1.100-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY native/ ./
COPY pgo-generator/ /training/generator/
ARG TRAIN_PROFILE=auto
# Training needs the same vector instructions as the submission binary.
# A cross-build on a machine without AVX-512 still produces the ranked ISA.
RUN set -eu; \
    target=x86_64-unknown-linux-musl; \
    if [ "$TRAIN_PROFILE" = 1 ] || { [ "$TRAIN_PROFILE" = auto ] && grep -qw avx512bw /proc/cpuinfo; }; then \
        rustup component add llvm-tools; \
        cargo run --locked --release --manifest-path /training/generator/Cargo.toml -- \
            --seed 987 --events 2000000 --clicks 500000 --threads 4 --output /training/data; \
        mkdir -p /training/raw /training/out; \
        cargo rustc --locked --release --target "$target" -- -Cprofile-generate=/training/raw; \
        LLVM_PROFILE_FILE=/training/raw/%p.profraw NCPU=4 DATA_DIR=/training/data OUTPUT_DIR=/training/out \
            "target/$target/release/shopping-sessions"; \
        "$(rustc --print sysroot)/lib/rustlib/$target/bin/llvm-profdata" merge \
            -o /training/profile /training/raw/*.profraw; \
        cargo rustc --locked --release --target "$target" -- -Cprofile-use=/training/profile; \
    else \
        cargo build --locked --release --target "$target"; \
    fi; \
    cp "target/$target/release/shopping-sessions" /pipeline

FROM scratch
COPY --from=build /pipeline /pipeline
COPY --from=build /pipeline /app/shopping-sessions
USER 1000:1000
CMD ["/pipeline"]
