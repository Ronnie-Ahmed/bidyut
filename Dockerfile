# ---------- build stage ----------
FROM rust:1.99-bookworm AS builder
WORKDIR /app

# Build dependencies first, so they are cached until Cargo.toml/Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

# Now the real source and migrations (migrate!() embeds them at compile time).
COPY src ./src
COPY migrations ./migrations
RUN touch src/main.rs && cargo build --release --locked

# ---------- runtime stage ----------
FROM debian:bookworm-slim

# Unprivileged user with no login shell and no home directory.
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin app

COPY --from=builder /app/target/release/bidyut /usr/local/bin/bidyut

USER app
EXPOSE 3000
CMD ["bidyut"]