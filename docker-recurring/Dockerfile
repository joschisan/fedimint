FROM rustlang/rust:nightly-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY . .

RUN cargo build --release --bin picomint-recurring-daemon

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 1000 picomint

COPY --from=builder /app/target/release/picomint-recurring-daemon /usr/local/bin/picomint-recurring-daemon

USER picomint

ENTRYPOINT ["picomint-recurring-daemon"]
