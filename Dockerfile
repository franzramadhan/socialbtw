FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Dependencies first, in their own cached layer: a change in src/ does not
# recompile rustls, postgres or the bundled SQLite.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src src
# touch: the copied main.rs can be older than the stub build above
RUN touch src/main.rs && cargo build --release --locked && mkdir /data

# Static musl binary with built-in CA roots (rustls + webpki-roots): nothing else needed.
FROM scratch
COPY --from=build /src/target/release/socialbtw /socialbtw
COPY --from=build --chown=10001:10001 /data /data
USER 10001:10001
ENV DB_PATH=/data/socialbtw.db
VOLUME /data
EXPOSE 9090
ENTRYPOINT ["/socialbtw"]
