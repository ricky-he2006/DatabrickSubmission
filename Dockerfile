FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY native/ ./
RUN cargo build --release --target x86_64-unknown-linux-musl \
 && cp target/x86_64-unknown-linux-musl/release/shopping-sessions /pipeline \
 && mkdir -p /app && cp /pipeline /app/shopping-sessions

FROM scratch
COPY --from=build /pipeline /pipeline
COPY --from=build /app /app
USER 1000:1000
CMD ["/pipeline"]
