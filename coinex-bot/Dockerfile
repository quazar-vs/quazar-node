FROM rust:1.96 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

# Match builder glibc (rust:1.96 needs GLIBC_2.39+; bookworm is too old).
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/coinex_martingale_bot /app/coinex_martingale_bot
ENV RUST_LOG=info
CMD ["/app/coinex_martingale_bot"]
