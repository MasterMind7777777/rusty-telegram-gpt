#!/bin/bash
# Deploy script for telegram_openai_bot service.
# This script pulls the latest code, rebuilds the binary,
# restarts the systemd service, and shows its status.
#
# Usage:
#   chmod +x deploy.sh
#   ./deploy.sh

# Exit immediately if a command exits with a non-zero status.
set -e

# Pull latest changes from the repository.
echo "==> Pulling latest code..."
git pull || { echo "Git pull failed!"; exit 1; }

# Build the binary in release mode.
echo "==> Building telegram_openai_bot binary..."
cargo build --release --bin telegram_openai_bot || { echo "Build failed!"; exit 1; }

# Restart the systemd service.
echo "==> Restarting telegram_openai_bot.service..."
sudo systemctl restart telegram_openai_bot.service || { echo "Failed to restart service!"; exit 1; }

# Show the status of the service.
echo "==> Showing service status..."
sudo systemctl status telegram_openai_bot.service
