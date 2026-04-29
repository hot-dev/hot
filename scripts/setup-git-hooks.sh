#!/bin/sh
# Script to set up Git hooks

# Path to this repository's .git/hooks directory
HOOKS_DIR=".git/hooks"
# Path to our custom hooks
CUSTOM_HOOKS_DIR="scripts/git-hooks"

# Make sure hooks directory exists
mkdir -p "$HOOKS_DIR"

# Copy the pre-push hook and make it executable
echo "Installing pre-commit hook..."
cp "$CUSTOM_HOOKS_DIR/pre-commit" "$HOOKS_DIR/pre-commit"
chmod +x "$HOOKS_DIR/pre-commit"

echo "Git hooks installed successfully!"