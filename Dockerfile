# reso-ingest container image: a static musl binary on an empty base.
#
#   docker build -t reso-ingest .
#   docker run --rm -v "$PWD/servers.toml:/config/servers.toml:ro" -v "$PWD/data:/data" \
#     -e MLS_ONE_TOKEN reso-ingest sync --config /config/servers.toml --out /data
#
# The only C code in the binary is `ring` (rustls' crypto), compiled here and linked in statically.
# TLS roots are built in (webpki-roots), so the image needs no CA bundle, libc or shell.

FROM rust:1.91-alpine AS build
# musl-dev: C headers and startup files for ring's build script
RUN apk add --no-cache musl-dev
WORKDIR /src
# Strip symbols: about a third smaller (Cargo.toml's release profile is left as is for local builds)
ENV CARGO_PROFILE_RELEASE_STRIP=symbols
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p reso-ingest --features parquet \
    && cp target/release/reso-ingest /reso-ingest \
    && mkdir -p /out/data

FROM scratch
COPY --from=build /reso-ingest /reso-ingest
# Writable working directory for the default --out (reso-out); mount a volume over /data
COPY --from=build --chown=65532:65532 /out/data /data
USER 65532:65532
WORKDIR /data
ENTRYPOINT ["/reso-ingest"]
CMD ["--help"]
