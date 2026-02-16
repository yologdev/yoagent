#!/bin/sh
# Install git hooks for yo-agent development
cp scripts/pre-commit .git/hooks/pre-commit
chmod +x .git/hooks/pre-commit
echo "✅ Git hooks installed."
