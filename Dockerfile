FROM rust:1-slim-bookworm AS builder
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked || cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 app \
    && mkdir -p /app/data /app/static /app/samples \
    && chown -R app:app /app
WORKDIR /app
COPY --from=builder /src/target/release/meteo_gpx /usr/local/bin/meteo_gpx
COPY static /app/static
USER app
ENV PORT=3000
ENV DB_PATH=/app/data/meteo.db
ENV SAMPLE_GPX_DIR=/app/samples
ENV RUST_LOG=info,meteo_gpx=info
EXPOSE 3000
CMD ["meteo_gpx"]
