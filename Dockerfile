FROM rust:1.85-slim-bookworm AS builder
WORKDIR /src
COPY native/Cargo.toml native/Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && cargo build --locked --release && rm -rf src
COPY native/src ./src
RUN touch src/main.rs && cargo build --release --locked
FROM debian:bookworm-slim
COPY --from=builder /src/target/release/shopping-sessions /app/native
CMD ["/app/native"]
