# MDA Runtime UI — build the Leptos WASM app with Trunk, serve the static bundle
# with nginx. Works with Docker or Podman.
#
#   podman build -t mda-ui web/runtime-ui
#   podman run --rm -p 8081:80 mda-ui   # http://localhost:8081
#
# The app calls the API at http://localhost:8080 by default (api.rs API_BASE).
# For non-local deployments, make API_BASE configurable (e.g. a window global /
# build-time env) — tracked as a follow-up.

FROM rust:1-bookworm AS build
RUN rustup target add wasm32-unknown-unknown && cargo install --locked trunk
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY index.html ./
RUN trunk build --release

FROM nginx:alpine
COPY --from=build /app/dist /usr/share/nginx/html
