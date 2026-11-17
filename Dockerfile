# syntax=docker/dockerfile:1

# 1. Dashboard
FROM node:22-bookworm-slim AS web
WORKDIR /web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml* web/pnpm-workspace.yaml* ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

# 2. Server (the dashboard is compiled into the binary)
FROM rust:1-bookworm AS server
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang && rm -rf /var/lib/apt/lists/*
WORKDIR /app
# REVISION (the git commit) is written by the deploy job; /healthz reports it.
COPY Cargo.toml Cargo.lock REVISION* ./
COPY src ./src
COPY migrations ./migrations
COPY --from=web /web/dist ./web/dist
RUN GIT_SHA="$(cat REVISION 2>/dev/null || true)" cargo build --release --locked

# 3. Runtime: one small binary
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 dumbmail
COPY --from=server /app/target/release/dumbmail /usr/local/bin/dumbmail
USER dumbmail
ENV BIND=0.0.0.0:8080
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080 && printf 'GET /healthz HTTP/1.0\\r\\n\\r\\n' >&3 && grep -q ok <&3"]
ENTRYPOINT ["dumbmail"]
CMD ["serve"]
