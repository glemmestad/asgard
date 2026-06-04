# Build the Docusaurus docs site; the binary embeds the output (docs/build) and
# serves it at /docs. Done in its own stage so the Rust build stays Node-free.
FROM node:20-bookworm AS docs
WORKDIR /docs
COPY docs/package.json docs/package-lock.json ./
RUN npm ci
COPY docs/ ./
RUN npm run build

# Build the single static binary. The web UI is a committed, build-free SPA in
# web/dist; both it and the docs site (copied in below) are embedded at compile
# time via rust-embed.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
COPY --from=docs /docs/build ./docs/build
RUN cargo build --release -p asgard

# Minimal runtime. SQLite default works with no extra setup; Postgres + the
# terraform connector (armed provisioning) work too — terraform is on PATH and
# the bundled modules are at /modules (point provisioning.terraform.modules_dir
# there in asgard.yaml).
FROM debian:bookworm-slim
ARG TF_VERSION=1.9.8
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl unzip \
    && arch="$(dpkg --print-architecture)" \
    && case "$arch" in \
         amd64) tfarch=amd64 ;; \
         arm64) tfarch=arm64 ;; \
         *) echo "unsupported arch: $arch" && exit 1 ;; \
       esac \
    && curl -fsSL "https://releases.hashicorp.com/terraform/${TF_VERSION}/terraform_${TF_VERSION}_linux_${tfarch}.zip" -o /tmp/tf.zip \
    && unzip /tmp/tf.zip -d /usr/local/bin \
    && rm /tmp/tf.zip \
    && apt-get purge -y unzip && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/asgard /usr/local/bin/asgard
# Bundled terraform modules (the connector resolves manifest `module` paths
# against modules_dir). Shipped in the image so a deploy needs no mounted tree.
COPY --from=build /src/modules /modules
RUN useradd --system --create-home asgard \
    && mkdir -p /data \
    && chown asgard:asgard /data
USER asgard
VOLUME /data
ENV ASGARD_DATABASE_URL=sqlite:///data/asgard.db
ENV ASGARD_BIND=0.0.0.0:8080
EXPOSE 8080
# Readiness: confirms the process is up and the database is reachable.
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:8080/readyz || exit 1
ENTRYPOINT ["asgard", "serve"]
