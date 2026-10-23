# syntax=docker/dockerfile:1

# The binary is compiled by the rust job in .github/workflows/ci.yml
FROM debian:trixie-slim
WORKDIR /app

# Artifact downloads drop the executable bit, so restore it here
COPY --chmod=755 target/release/icon-market /usr/local/bin/icon-market
# The server resolves static/ relative to its working directory
COPY static ./static

USER 10001:10001
EXPOSE 3000
CMD ["icon-market"]
