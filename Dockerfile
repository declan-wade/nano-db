FROM rust:1.89-slim-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN mkdir -p /data && chown 10001:10001 /data
COPY --from=build /app/target/release/nano-db /usr/local/bin/nano-db
USER 10001:10001
ENV NANO_DB_BIND=0.0.0.0:7070
ENV NANO_DB_PATH=/data/nano-db.redb
EXPOSE 7070
ENTRYPOINT ["/usr/local/bin/nano-db"]
