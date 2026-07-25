FROM rust:1.82-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release
FROM debian:bookworm-slim
COPY --from=build /src/target/release/telegram_collector /usr/local/bin/telegram_collector
ENTRYPOINT ["telegram_collector"]
