# The status page, embedded into the hub binary by the build stage (`--features ui`).
FROM node:22-bookworm-slim@sha256:c3de60bf2f9dd0ac6370e6117950ff62d6e339527e7472301c9c78a017978392 AS ui
WORKDIR /ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci --ignore-scripts
COPY ui ./
RUN npm run build

FROM rust:1.96.0-slim-bookworm@sha256:4732ca96fd086cb9be682050c3f0176288eebaac2b80aa2bcefccfaf198e1950 AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY LICENSE THIRD_PARTY_NOTICES.md ./
COPY src ./src
COPY --from=ui /ui/dist ./ui/dist
COPY tools/portable-verifier ./tools/portable-verifier
RUN cargo build --locked --release --features ui

# Test-only seeder for .github/hub-e2e.sh. `docker build .` builds only the last stage, so it never
# builds or ships these two.
FROM build AS e2e-seed-build
COPY examples/hub_seed.rs ./examples/hub_seed.rs
RUN cargo build --locked --release --features ui --example hub_seed

FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS e2e-seed
RUN groupadd --gid 10001 agentscm \
    && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /nonexistent agentscm \
    && mkdir /data /tokens \
    && chown agentscm:agentscm /data /tokens
COPY --from=e2e-seed-build /build/target/release/examples/hub_seed /usr/local/bin/hub-seed
USER 10001:10001
ENTRYPOINT ["hub-seed"]

# trixie, not bookworm: `agentscm collisions` needs Git 2.43 or newer.
FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS runtime
LABEL org.opencontainers.image.licenses="FSL-1.1-ALv2" \
      org.opencontainers.image.source="https://github.com/counterbranch/agentscm" \
      org.opencontainers.image.vendor="DUVATL, Inc."
RUN apt-get update \
    && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 agentscm \
    && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /nonexistent agentscm \
    && mkdir /data \
    && chown agentscm:agentscm /data
COPY --from=build /build/target/release/agentscm /usr/local/bin/agentscm
COPY LICENSE NOTICE THIRD_PARTY_NOTICES.md /usr/share/doc/agentscm/
USER 10001:10001
VOLUME ["/data"]
EXPOSE 7331
ENTRYPOINT ["agentscm"]
CMD ["--data-dir", "/data", "serve", "--bind", "0.0.0.0:7331", "--public", "--max-concurrency", "1"]
