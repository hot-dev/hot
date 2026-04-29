#!/bin/bash

set -e

# Function to strip markdown formatting for plain text display
# Converts LICENSE.md to readable plain text for installer license pages
strip_markdown() {
    local input="$1"
    local output="$2"

    # Use perl for cross-platform regex compatibility (available on macOS and Linux)
    perl -pe '
        s/^#+ //;                           # Remove header markers
        s/\*\*([^*]+)\*\*/$1/g;            # Remove bold markers
        s/\*([^*]+)\*/$1/g;                # Remove italic markers
        s/`([^`]+)`/$1/g;                  # Remove code markers
        s/\[([^\]]+)\]\(\1\)/$1/g;         # Remove redundant links where text = url
        s/\[([^\]]+)\]\(([^)]+)\)/$1 ($2)/g;  # Convert other links to "text (url)"
        s/^---$/________________________________________/;  # Horizontal rules
        s/^- /  • /;                        # Bullet points
    ' "$input" > "$output"
}

# Check for required macOS packaging tools
if ! command -v pkgbuild &> /dev/null; then
    echo "Error: pkgbuild command not found. This script requires macOS developer tools."
    exit 1
fi

if ! command -v productbuild &> /dev/null; then
    echo "Error: productbuild command not found. This script requires macOS developer tools."
    exit 1
fi

# Save current directory
ORIGINAL_DIR=$(pwd)

# Change to workspace root
cd "$(dirname "$0")/../"

# Extract version from resources/version.txt (single source of truth)
if [ ! -f "resources/version.txt" ]; then
    echo "Error: resources/version.txt not found"
    exit 1
fi
VERSION=$(head -1 resources/version.txt | tr -d '[:space:]')

if [ -z "$VERSION" ]; then
    echo "Error: Could not extract version from resources/version.txt"
    exit 1
fi

echo "Building Hot Dev packages for version $VERSION..."

# Apple targets to package
TARGETS=(
    "aarch64-apple-darwin"
    "x86_64-apple-darwin"
)

# Create packages directory
PACKAGES_DIR="target/packages"
mkdir -p "$PACKAGES_DIR"

for TARGET in "${TARGETS[@]}"; do
    echo "Packaging for target: $TARGET"

    BINARY_PATH="target/$TARGET/release/hot"

    # Check if binary exists
    if [ ! -f "$BINARY_PATH" ]; then
        echo "Error: Binary not found at $BINARY_PATH"
        echo "Please run: cargo build --release --target $TARGET"
        exit 1
    fi

    # Create staging directory
    STAGING_DIR="target/staging/$TARGET"
    rm -rf "$STAGING_DIR"
    mkdir -p "$STAGING_DIR/payload/usr/local/bin"
    mkdir -p "$STAGING_DIR/payload/usr/local/share/hot"
    mkdir -p "$STAGING_DIR/payload/usr/local/share/hot/scripts"
    mkdir -p "$STAGING_DIR/scripts"
    mkdir -p "$STAGING_DIR/installer-resources"

    # Copy binary
    cp "$BINARY_PATH" "$STAGING_DIR/payload/usr/local/bin/hot"
    chmod +x "$STAGING_DIR/payload/usr/local/bin/hot"

    # Copy resources needed at runtime:
    # - db/ for migrations
    # - pkg/ for hot-std (standard library)
    # - ai/ for AI AGENTS.md and skills
    # - init/ for project initialization templates
    # - LICENSE.md for license display

    # Copy database migrations
    if [ -d "resources/db" ]; then
        mkdir -p "$STAGING_DIR/payload/usr/local/share/hot/db"
        cp -r resources/db/* "$STAGING_DIR/payload/usr/local/share/hot/db/"
    fi

    # Copy hot-std package (standard library)
    if [ -d "hot/pkg/hot-std" ]; then
        mkdir -p "$STAGING_DIR/payload/usr/local/share/hot/pkg"
        cp -r hot/pkg/hot-std "$STAGING_DIR/payload/usr/local/share/hot/pkg/"
    fi

    # Copy app assets (CSS, JS, images for dev server)
    if [ -d "resources/app" ]; then
        mkdir -p "$STAGING_DIR/payload/usr/local/share/hot/app"
        cp -r resources/app/* "$STAGING_DIR/payload/usr/local/share/hot/app/"
    fi

    # Copy AI resources (AGENTS.md and skills)
    if [ -d "resources/ai" ]; then
        mkdir -p "$STAGING_DIR/payload/usr/local/share/hot/ai"
        cp -r resources/ai/* "$STAGING_DIR/payload/usr/local/share/hot/ai/"
    fi

    # Copy init templates
    if [ -d "resources/init" ]; then
        mkdir -p "$STAGING_DIR/payload/usr/local/share/hot/init"
        cp -r resources/init/* "$STAGING_DIR/payload/usr/local/share/hot/init/"
    fi

    # Copy hotbox Linux binaries (for container tasks via hot dev)
    if ls resources/bin/hotbox-linux-* 1>/dev/null 2>&1; then
        mkdir -p "$STAGING_DIR/payload/usr/local/share/hot/bin"
        cp resources/bin/hotbox-linux-* "$STAGING_DIR/payload/usr/local/share/hot/bin/"
        chmod +x "$STAGING_DIR/payload/usr/local/share/hot/bin/hotbox-linux-"*
    fi

    # Copy license and notice files
    if [ -f "LICENSE" ]; then
        cp LICENSE "$STAGING_DIR/payload/usr/local/share/hot/LICENSE"
    fi
    if [ -f "NOTICE" ]; then
        cp NOTICE "$STAGING_DIR/payload/usr/local/share/hot/NOTICE"
    fi

    # Copy install-completions script into package payload
    cp scripts/install-completions.sh "$STAGING_DIR/payload/usr/local/share/hot/scripts/install-completions.sh"
    chmod +x "$STAGING_DIR/payload/usr/local/share/hot/scripts/install-completions.sh"

    # Copy license file for installer
    if [ -f "LICENSE" ]; then
        cp LICENSE "$STAGING_DIR/installer-resources/LICENSE.txt"
    fi

    # Copy installer branding resources
    if [ -f "resources/installer/mac/background.png" ]; then
        cp "resources/installer/mac/background.png" "$STAGING_DIR/installer-resources/"
    fi
    if [ -f "resources/installer/mac/background-darkAqua.png" ]; then
        cp "resources/installer/mac/background-darkAqua.png" "$STAGING_DIR/installer-resources/"
    fi
    if [ -f "resources/installer/mac/welcome.html" ]; then
        cp "resources/installer/mac/welcome.html" "$STAGING_DIR/installer-resources/"
    fi
    if [ -f "resources/installer/mac/conclusion.html" ]; then
        cp "resources/installer/mac/conclusion.html" "$STAGING_DIR/installer-resources/"
    fi

    # Create postinstall script to add to PATH and install completions
    cat > "$STAGING_DIR/scripts/postinstall" << 'EOF'
#!/bin/bash

# Add /usr/local/bin to PATH for all users if not already present
if ! grep -q '/usr/local/bin' /etc/paths; then
    echo '/usr/local/bin' >> /etc/paths
fi

# Set permissions
chmod +x /usr/local/bin/hot

# Install shell completions system-wide (ignore errors)
if [ -x /usr/local/share/hot/scripts/install-completions.sh ]; then
  HOT_BIN=/usr/local/bin/hot HOT_INSTALL_SCOPE=system /usr/local/share/hot/scripts/install-completions.sh || true
fi

exit 0
EOF
    chmod +x "$STAGING_DIR/scripts/postinstall"

    # Sign the binary BEFORE packaging (if signing is enabled)
    if [ -n "$APPLE_DEVELOPER_ID_APP" ]; then
        echo "Signing binary with Developer ID..."
        codesign --force --options runtime \
            --sign "$APPLE_DEVELOPER_ID_APP" \
            --timestamp \
            "$STAGING_DIR/payload/usr/local/bin/hot"

        echo "Verifying signature..."
        codesign --verify --verbose "$STAGING_DIR/payload/usr/local/bin/hot"
    fi

    # Build component package
    COMPONENT_PKG="$PACKAGES_DIR/hot-component-$TARGET.pkg"
    pkgbuild \
        --root "$STAGING_DIR/payload" \
        --scripts "$STAGING_DIR/scripts" \
        --identifier "dev.hot.hot.pkg" \
        --version "$VERSION" \
        --install-location "/" \
        "$COMPONENT_PKG"

    # Create distribution.xml for productbuild
    cat > "$STAGING_DIR/distribution.xml" << EOF
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>Hot Dev $VERSION</title>

    <!-- Branding -->
    <background file="background.png" alignment="center" scaling="proportional"/>
    <background-darkAqua file="background-darkAqua.png" alignment="center" scaling="proportional"/>

    <!-- Installer pages -->
    <welcome file="welcome.html" mime-type="text/html"/>
    <license file="LICENSE.txt" mime-type="text/plain"/>
    <conclusion file="conclusion.html" mime-type="text/html"/>

    <pkg-ref id="dev.hot.hot.pkg"/>

    <options customize="never" require-scripts="false"/>
    <choices-outline>
        <line choice="default">
            <line choice="dev.hot.hot.pkg"/>
        </line>
    </choices-outline>

    <choice id="default"/>
    <choice id="dev.hot.hot.pkg" visible="false">
        <pkg-ref id="dev.hot.hot.pkg"/>
    </choice>

    <pkg-ref id="dev.hot.hot.pkg" version="$VERSION" onConclusion="none">hot-component-$TARGET.pkg</pkg-ref>
</installer-gui-script>
EOF

    # Build final product package
    FINAL_PKG="$PACKAGES_DIR/hot_${VERSION}_${TARGET}.pkg"
    productbuild \
        --distribution "$STAGING_DIR/distribution.xml" \
        --resources "$STAGING_DIR/installer-resources" \
        --package-path "$PACKAGES_DIR" \
        --version "$VERSION" \
        "$FINAL_PKG"

    # Clean up component package (keep only final package)
    rm "$COMPONENT_PKG"

    # ===========================================
    # Code Signing and Notarization (Optional)
    # ===========================================
    # To enable signing, set these environment variables:
    #   APPLE_DEVELOPER_ID_APP    - Developer ID Application certificate name
    #   APPLE_DEVELOPER_ID_INST   - Developer ID Installer certificate name
    #   APPLE_ID                  - Apple ID email for notarization
    #   APPLE_APP_PASSWORD        - App-specific password for notarization
    #   APPLE_TEAM_ID             - Apple Developer Team ID
    #
    # Example:
    #   export APPLE_DEVELOPER_ID_APP="Developer ID Application: Your Name (TEAMID)"
    #   export APPLE_DEVELOPER_ID_INST="Developer ID Installer: Your Name (TEAMID)"
    #   export APPLE_ID="your@email.com"
    #   export APPLE_APP_PASSWORD="xxxx-xxxx-xxxx-xxxx"
    #   export APPLE_TEAM_ID="TEAMID"

    if [ -n "$APPLE_DEVELOPER_ID_INST" ]; then
        echo "Signing package with Developer ID Installer..."
        SIGNED_PKG="${FINAL_PKG%.pkg}_signed.pkg"
        productsign --sign "$APPLE_DEVELOPER_ID_INST" "$FINAL_PKG" "$SIGNED_PKG"
        mv "$SIGNED_PKG" "$FINAL_PKG"
        echo "Package signed successfully."

        # Notarize if credentials are available
        if [ -n "$APPLE_ID" ] && [ -n "$APPLE_APP_PASSWORD" ] && [ -n "$APPLE_TEAM_ID" ]; then
            echo "Submitting for notarization..."

            # Stream output live via tee so we still see it on hang/timeout/crash,
            # while also capturing it for submission-id extraction. Disable set -e
            # locally so a non-zero exit doesn't skip our error-handling block
            # (which fetches the detailed notarization log from Apple).
            NOTARIZE_LOG=$(mktemp)
            set +e
            xcrun notarytool submit "$FINAL_PKG" \
                --apple-id "$APPLE_ID" \
                --password "$APPLE_APP_PASSWORD" \
                --team-id "$APPLE_TEAM_ID" \
                --verbose \
                --wait 2>&1 | tee "$NOTARIZE_LOG"
            NOTARIZE_STATUS=${PIPESTATUS[0]}
            set -e
            NOTARIZE_OUTPUT=$(cat "$NOTARIZE_LOG")
            rm -f "$NOTARIZE_LOG"

            # Extract submission ID from output
            SUBMISSION_ID=$(echo "$NOTARIZE_OUTPUT" | grep -E "^\s*id:" | head -1 | awk '{print $2}')

            if [ "$NOTARIZE_STATUS" -ne 0 ] || echo "$NOTARIZE_OUTPUT" | grep -q "status: Invalid"; then
                echo ""
                echo "Notarization failed (exit status: $NOTARIZE_STATUS, submission id: ${SUBMISSION_ID:-<none>})."
                if [ -n "$SUBMISSION_ID" ]; then
                    echo "Fetching detailed notarization log from Apple..."
                    xcrun notarytool log "$SUBMISSION_ID" \
                        --apple-id "$APPLE_ID" \
                        --password "$APPLE_APP_PASSWORD" \
                        --team-id "$APPLE_TEAM_ID" || \
                        echo "(failed to fetch notarytool log for $SUBMISSION_ID)"
                else
                    echo "No submission id was returned by notarytool — this usually indicates a"
                    echo "transport-level failure (network/upload error or Apple service outage)"
                    echo "rather than a problem with the package itself. Check Apple System Status:"
                    echo "  https://www.apple.com/support/systemstatus/"
                fi
                exit 1
            fi

            echo "Stapling notarization ticket..."
            xcrun stapler staple "$FINAL_PKG"
            echo "Notarization complete."
        else
            echo "Skipping notarization (APPLE_ID, APPLE_APP_PASSWORD, or APPLE_TEAM_ID not set)"
        fi
    else
        echo "Skipping code signing (APPLE_DEVELOPER_ID_APP or APPLE_DEVELOPER_ID_INST not set)"
    fi

    echo "Created: $FINAL_PKG"
done

echo ""
echo "Package creation completed successfully!"
echo "Packages created in: $PACKAGES_DIR"
ls -la "$PACKAGES_DIR"/*.pkg

# Restore original directory
cd "$ORIGINAL_DIR"