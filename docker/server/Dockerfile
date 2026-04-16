FROM rustlang/rust:nightly-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY . .

RUN cargo build --release --bin picomint-server-daemon --bin picomint-server-cli

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 1000 picomint \
    && mkdir -p /data \
    && chown -R picomint:picomint /data

COPY --from=builder /app/target/release/picomint-server-daemon /usr/local/bin/picomint-server-daemon
COPY --from=builder /app/target/release/picomint-server-cli /usr/local/bin/picomint-server-cli

USER picomint

ENTRYPOINT ["picomint-server-daemon"]
