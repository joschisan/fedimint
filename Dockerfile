FROM rust:1.89-bookworm as builder
RUN apt-get update && apt-get install -y cmake clang libclang-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY . .
RUN cargo build --release -p fedimint-recurringdv2

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/fedimint-recurringdv2 /usr/local/bin/
CMD ["fedimint-recurringdv2"]
