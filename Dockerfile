# syntax=docker/dockerfile:1
#
# Two stages. The runtime is debian-slim rather than scratch or distroless,
# because arbor shells out to `git` for cloning, history and change detection —
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
 && strip target/release/arbor

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Unprivileged: the agent has no tools, but the process still clones untrusted
# repositories and runs a parser over their contents.
RUN useradd --system --create-home --uid 10001 arbor
USER arbor

COPY --from=build --chown=arbor:arbor /src/target/release/arbor /usr/local/bin/arbor

# Everything mutable lives here: clones, graphs, the database. One volume is the
# whole backup story.
ENV ARBOR_DATA=/data
VOLUME ["/data"]
WORKDIR /data

EXPOSE 7777

# 0.0.0.0 because the container's loopback is not reachable from outside it.
# Put a reverse proxy in front; the webhook endpoint expects to be public but
# the repo-management API does not.
ENTRYPOINT ["arbor"]
CMD ["server", "--addr", "0.0.0.0:7777", "--data", "/data"]

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD ["arbor", "health", "--addr", "127.0.0.1:7777"]
