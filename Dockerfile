FROM rust:1.88-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential cmake git libclang-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release --locked --bin redlib-mcp

FROM debian:bookworm-slim

COPY --from=builder /app/target/release/redlib-mcp /usr/local/bin/redlib-mcp
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
USER 65532:65532

EXPOSE 8080
CMD ["redlib-mcp"]
