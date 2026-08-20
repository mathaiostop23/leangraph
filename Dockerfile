# syntax=docker/dockerfile:1
#
# Two stages. The runtime is debian-slim rather than scratch or distroless,
# because leangraph shells out to `git` for cloning, history and change detection —
# an honest runtime dependency that earlier drafts of SERVER.md glossed over by
# claiming a static binary with nothing to ship alongside it.

FROM rust:1-bookworm AS build
WORKDIR /src

# Dependency layer first, so a source-only change does not rebuild ~200 crates.
# The dummy main is replaced below; touching main.rs forces cargo to notice.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --locked \
 && strip target/release/leangraph

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Unprivileged: the agent has no tools, but the process still clones untrusted
# repositories and runs a parser over their contents.
RUN useradd --system --create-home --uid 10001 leangraph
USER leangraph

COPY --from=build --chown=leangraph:leangraph /src/target/release/leangraph /usr/local/bin/leangraph

# Everything mutable lives here: clones, graphs, the database. One volume is the
# whole backup story.
ENV LEANGRAPH_DATA=/data
VOLUME ["/data"]
WORKDIR /data

EXPOSE 7777

# 0.0.0.0 because the container's loopback is not reachable from outside it.
# Put a reverse proxy in front; the webhook endpoint expects to be public but
# the repo-management API does not.
ENTRYPOINT ["leangraph"]
CMD ["server", "--addr", "0.0.0.0:7777", "--data", "/data"]

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD ["leangraph", "health", "--addr", "127.0.0.1:7777"]
