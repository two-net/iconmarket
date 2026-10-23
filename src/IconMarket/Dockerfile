# syntax=docker/dockerfile:1

# The binary is compiled by the rust job in .github/workflows/ci.yml
FROM debian:trixie-slim
# Mount the config file at /app/iconmarket.toml; it holds credentials, so it is never baked in
WORKDIR /app

# Artifact downloads drop the executable bit, so restore it here
COPY --chmod=755 target/release/icon-market /usr/local/bin/icon-market
# The server resolves static/ relative to its working directory
COPY static ./static

USER 10001:10001
EXPOSE 8080
# Exec form, so SIGTERM reaches the server for a graceful shutdown
CMD ["icon-market"]
