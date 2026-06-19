#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

echo "Building sandbox-shell-mcp..."
cargo install --force --path .
echo "Done. Restart Claude Code to reload the MCP server."
