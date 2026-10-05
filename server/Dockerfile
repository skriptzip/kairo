# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /src
# Build dependencies first so source changes don't rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked && rm -rf src target/release/kairo-server*
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /data kairo && mkdir /data && chown kairo /data
COPY --from=build /src/target/release/kairo-server /usr/local/bin/kairo-server
USER kairo
ENV KAIRO_ADDR=0.0.0.0:8080 KAIRO_DB=/data/kairo.db
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["kairo-server", "healthcheck"]
ENTRYPOINT ["kairo-server"]
