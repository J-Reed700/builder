# syntax=docker/dockerfile:1.7
FROM rust:1.95-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /source
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/source/target \
    cargo build --release --locked -p builder-gateway && \
    cp target/release/builder-gateway /builder-gateway

FROM alpine:3.22
RUN addgroup -S builder && adduser -S -G builder -h /data builder
COPY --from=build /builder-gateway /usr/local/bin/builder-gateway
RUN mkdir -p /data && chown builder:builder /data
USER builder
ENV BUILDER_GATEWAY_LISTEN=0.0.0.0:8080
ENV BUILDER_GATEWAY_DATA=/data
EXPOSE 8080
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=3s CMD wget -q -O /dev/null http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["builder-gateway"]
