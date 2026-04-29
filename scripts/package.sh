#!/bin/bash

### Create packages for all supported platforms

set -e

# Save current directory
ORIGINAL_DIR=$(pwd)

# Change to script directory
cd "$(dirname "$0")"

echo "=========================================="
echo "Hot Dev - Package Creation"
echo "=========================================="
echo ""

# Initialize result tracking
LINUX_SUCCESS=false
MAC_SUCCESS=false

# Extract version from Cargo.toml for display
cd ../
VERSION=$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
cd scripts

if [ -n "$VERSION" ]; then
    echo "Creating packages for Hot Dev version: $VERSION"
else
    echo "Creating packages for Hot Dev"
fi
echo ""

# Run Linux packaging
echo "=========================================="
echo "1. Linux Packaging (.deb and .rpm)"
echo "=========================================="
if [ -f "./package-linux.sh" ]; then
    if ./package-linux.sh; then
        echo "✅ Linux packaging completed successfully"
        LINUX_SUCCESS=true
    else
        echo "❌ Linux packaging failed"
        echo "Note: This may be expected if Docker is not available or Linux builds don't exist"
    fi
else
    echo "❌ package-linux.sh not found"
fi

echo ""

# Run macOS packaging
echo "=========================================="
echo "2. macOS Packaging (.pkg)"
echo "=========================================="
if [ -f "./package-mac.sh" ]; then
    if ./package-mac.sh; then
        echo "✅ macOS packaging completed successfully"
        MAC_SUCCESS=true
    else
        echo "❌ macOS packaging failed"
        echo "Note: This may be expected if macOS developer tools are not available or macOS builds don't exist"
    fi
else
    echo "❌ package-mac.sh not found"
fi

echo ""

# Run Windows packaging
echo "=========================================="
echo "3. Windows Packaging (.exe installer)"
echo "=========================================="
WINDOWS_SUCCESS=false
if [ -f "./package-win.sh" ]; then
    if ./package-win.sh; then
        echo "✅ Windows packaging completed successfully"
        WINDOWS_SUCCESS=true
    else
        echo "❌ Windows packaging failed"
        echo "Note: This may be expected if Docker is not available or Windows builds don't exist"
    fi
else
    echo "❌ package-win.sh not found"
fi

echo ""

# Summary
echo "=========================================="
echo "Packaging Summary"
echo "=========================================="

# Count successful packaging
TOTAL_SUCCESS=0
if [ "$LINUX_SUCCESS" = true ]; then ((TOTAL_SUCCESS++)); fi
if [ "$MAC_SUCCESS" = true ]; then ((TOTAL_SUCCESS++)); fi
if [ "$WINDOWS_SUCCESS" = true ]; then ((TOTAL_SUCCESS++)); fi

if [ $TOTAL_SUCCESS -eq 3 ]; then
    echo "🎉 All packaging completed successfully!"
    echo ""
    echo "Created packages in target/packages/:"
    ls -la "../target/packages/" 2>/dev/null | grep -E '\.(pkg|deb|rpm|exe)$' || echo "  (no packages found)"
elif [ $TOTAL_SUCCESS -gt 0 ]; then
    echo "⚠️  Partial packaging success ($TOTAL_SUCCESS/3 platforms)"
    echo ""
    if [ "$LINUX_SUCCESS" = true ]; then
        echo "✅ Linux packaging completed successfully"
    else
        echo "❌ Linux packaging failed"
    fi
    if [ "$MAC_SUCCESS" = true ]; then
        echo "✅ macOS packaging completed successfully"
    else
        echo "❌ macOS packaging failed"
    fi
    if [ "$WINDOWS_SUCCESS" = true ]; then
        echo "✅ Windows packaging completed successfully"
    else
        echo "❌ Windows packaging failed"
    fi
    echo ""
    echo "Created packages in target/packages/:"
    ls -la "../target/packages/" 2>/dev/null | grep -E '\.(pkg|deb|rpm|exe)$' || echo "  (none found)"
else
    echo "❌ All packaging failed"
    echo ""
    echo "Common issues:"
    echo "  - Linux: Docker not installed or Linux builds missing (run build-linux.sh first)"
    echo "  - macOS: Developer tools not installed or macOS builds missing (run build for macOS targets first)"
    echo "  - Windows: PowerShell not available or Windows builds missing"
    exit 1
fi

echo ""
echo "=========================================="

# Restore original directory
cd "$ORIGINAL_DIR"