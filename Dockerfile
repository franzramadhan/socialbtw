FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release --locked && mkdir /data

# Static musl binary with built-in CA roots (rustls + webpki-roots): nothing else needed.
FROM scratch
COPY --from=build /src/target/release/socialbtw /socialbtw
COPY --from=build --chown=10001:10001 /data /data
USER 10001:10001
ENV DB_PATH=/data/socialbtw.db
VOLUME /data
EXPOSE 9090
ENTRYPOINT ["/socialbtw"]
