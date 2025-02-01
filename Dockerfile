# Stage 1: Build the binary
FROM rust:1.83 as builder
WORKDIR /usr/src/telegram_openai_bot
# Copy only the manifests first to cache dependencies.
COPY Cargo.toml Cargo.lock ./
# Create a dummy src file so that cargo can build dependencies.
RUN mkdir src && echo "fn main() {}" > src/main.rs
# Build with release profile (this will cache dependencies)
RUN cargo build --release --bin telegram_openai_bot
# Remove the dummy main file and copy the full source.
RUN rm -f src/main.rs
COPY . .
# Now rebuild the binary with your actual code.
RUN cargo build --release --bin telegram_openai_bot

# Stage 2: Create a minimal runtime image.
FROM debian:bookworm-slim
# Install CA certificates for HTTPS requests.
RUN apt-get update && apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*
WORKDIR /app
# Copy the compiled binary and the .env file into the runtime image.
COPY --from=builder /usr/src/telegram_openai_bot/target/release/telegram_openai_bot .
COPY .env .
# Expose the port your server listens on (read from .env or default to 8080)
EXPOSE 8888
# Run the webhook receiver.
CMD ["./telegram_openai_bot"]
