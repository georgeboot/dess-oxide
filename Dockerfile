# syntax=docker/dockerfile:1
# Image for the Home Assistant app. CI builds it natively per architecture
# (amd64, arm64) and publishes a multi-arch manifest to ghcr.io.

FROM rust:1.97-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p dess-oxide \
    && cp target/release/dess-oxide /dess-oxide

FROM alpine:3.23
COPY --from=build /dess-oxide /usr/local/bin/dess-oxide
LABEL io.hass.type="addon" \
      org.opencontainers.image.source="https://github.com/georgeboot/dess-oxide" \
      org.opencontainers.image.description="Plans and runs a Victron ESS against Dutch day-ahead prices"
ENTRYPOINT ["dess-oxide"]
CMD ["run", "--config", "/data/options.json", "--data-dir", "/data"]
