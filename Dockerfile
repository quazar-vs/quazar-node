FROM rust:1.96 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

# Match builder glibc (rust:1.96 needs GLIBC_2.39+; bookworm is too old).
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3t64 curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/quazar_registry /app/
COPY --from=builder /app/target/release/quazar_crypto_agent /app/
EXPOSE 8080
ENV DATABASE_URL=postgres://quazar:quazar@postgres:5432/quazar
ENV QUAZAR_NODE_ID=QZ-NODE
ENV QUAZAR_NODE_URL=http://localhost:8080
ENV PASSPORT_DOWNLOAD_URL=https://github.com/quazar-vs/quazar-passport/releases
ENV QUAZAR_PORT=8080
CMD ["/app/quazar_registry"]
