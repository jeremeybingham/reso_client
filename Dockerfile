# reso-ingest container image: a static musl binary on an empty base.
#
#   docker build -t reso-ingest .
#   docker run --rm -v "$PWD/servers.toml:/config/servers.toml:ro" -v "$PWD/data:/data" \
#     -e MLS_ONE_TOKEN reso-ingest sync --config /config/servers.toml --out /data
#
# The binary is built with `parquet`, `s3` and the default `mimalloc`. Its only C code is two crypto
# libraries and the allocator, all compiled here and linked in statically: `ring` (rustls for RESO
# servers and the webhook), `aws-lc` (rustls inside `object_store`, used by `--s3-*` and
# `--gcs-bucket`) and mimalloc (the global allocator, in place of musl's malloc).
# TLS roots: RESO servers and the webhook use roots built into the binary (webpki-roots). The S3
# and GCS clients check certificates against the system CA bundle, so the image carries Alpine's
# /etc/ssl/certs/ca-certificates.crt; mount your own file over it for a store with a private CA.
# No libc or shell.

FROM rust:1.91-alpine AS build
# musl-dev: C headers and startup files for the ring and aws-lc build scripts;
# ca-certificates: the CA bundle copied into the image for the S3 client
RUN apk add --no-cache musl-dev ca-certificates
WORKDIR /src
# Strip symbols: about a third smaller (Cargo.toml's release profile is left as is for local builds)
ENV CARGO_PROFILE_RELEASE_STRIP=symbols
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p reso-ingest --features parquet,s3 \
    && cp target/release/reso-ingest /reso-ingest \
    && mkdir -p /out/data

FROM scratch
COPY --from=build /reso-ingest /reso-ingest
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
# Writable working directory for the default --out (reso-out); mount a volume over /data
COPY --from=build --chown=65532:65532 /out/data /data
USER 65532:65532
WORKDIR /data
ENTRYPOINT ["/reso-ingest"]
CMD ["--help"]
