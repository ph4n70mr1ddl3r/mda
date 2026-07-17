# MDA Runtime UI — build the Leptos WASM app with Trunk, serve the static bundle
# with nginx. Works with Docker or Podman.
#
#   podman build -t mda-ui web/runtime-ui
#   podman run --rm -p 8081:80 mda-ui   # http://localhost:8081
#
# The app calls the API at http://localhost:8080 by default (api.rs API_BASE).
# For non-local deployments, make API_BASE configurable (e.g. a window global /
# build-time env) — tracked as a follow-up.
#
# Rust + Trunk are pinned (not floating) for reproducible builds that match the
# `ui` CI job; bump them deliberately (ideally via Dependabot/Renovate).

FROM rust:1.96.0-bookworm AS build
RUN rustup target add wasm32-unknown-unknown \
 && cargo install --locked --version 0.21.8 trunk
WORKDIR /app

# 1) Deps layer: copy manifests + a throwaway lib and build external deps once,
#    so later `src` changes don't recompile leptos/gloo/web-sys/etc.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo '' > src/lib.rs \
 && cargo build --release --target wasm32-unknown-unknown --lib

# 2) Real source + Trunk entrypoint → bundle (reuses the cached deps above).
COPY src ./src
COPY index.html ./
RUN trunk build --release

FROM nginx:alpine
COPY --from=build /app/dist /usr/share/nginx/html
