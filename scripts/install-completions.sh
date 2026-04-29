#!/usr/bin/env bash
set -euo pipefail

# Install shell completions for the hot CLI.
# Supports per-user (default) and system scope (HOT_INSTALL_SCOPE=system or run as root).

BIN="${HOT_BIN:-hot}"
OS="$(uname -s)"
SCOPE="${HOT_INSTALL_SCOPE:-}"

is_root() { [ "${EUID:-$(id -u)}" -eq 0 ]; }

use_system_paths=false
if is_root || [ "$SCOPE" = "system" ]; then
  use_system_paths=true
fi

case "$OS" in
  Darwin)
    # Zsh
    if command -v zsh >/dev/null 2>&1; then
      if $use_system_paths; then
        ZSH_DIR="/usr/local/share/zsh/site-functions"
        mkdir -p "$ZSH_DIR" || true
        "$BIN" completions zsh > "$ZSH_DIR/_hot" || true
        echo "Installed Zsh completion to $ZSH_DIR/_hot" || true
      else
        ZSH_DIR="$HOME/.zsh/completions"
        mkdir -p "$ZSH_DIR"
        "$BIN" completions zsh > "$ZSH_DIR/_hot"
        echo "Installed Zsh completion to $ZSH_DIR/_hot"
        # Ensure fpath is configured by user; we won't modify shell files here
      fi
    fi

    # Bash
    if command -v bash >/dev/null 2>&1; then
      if $use_system_paths; then
        if [ -d "/usr/local/etc/bash_completion.d" ]; then
          "$BIN" completions bash > "/usr/local/etc/bash_completion.d/hot" || true
          echo "Installed Bash completion to /usr/local/etc/bash_completion.d/hot" || true
        fi
      else
        mkdir -p "$HOME/.bash_completion.d"
        "$BIN" completions bash > "$HOME/.bash_completion.d/hot"
        if ! grep -q "\.\s\+\$HOME/.bash_completion.d/hot" "$HOME/.bash_profile" 2>/dev/null; then
          echo ". $HOME/.bash_completion.d/hot" >> "$HOME/.bash_profile"
        fi
        echo "Installed Bash completion to $HOME/.bash_completion.d/hot (sourced from ~/.bash_profile)"
      fi
    fi

    # Fish
    if command -v fish >/dev/null 2>&1; then
      if $use_system_paths; then
        # Try vendor completions; ignore if not present
        for d in /usr/local/share/fish/vendor_completions.d /usr/share/fish/vendor_completions.d; do
          if [ -d "$d" ]; then
            "$BIN" completions fish > "$d/hot.fish" || true
            echo "Installed Fish completion to $d/hot.fish" || true
            break
          fi
        done
      else
        FISH_DIR="$HOME/.config/fish/completions"
        mkdir -p "$FISH_DIR"
        "$BIN" completions fish > "$FISH_DIR/hot.fish"
        echo "Installed Fish completion to $FISH_DIR/hot.fish"
      fi
    fi
    ;;
  Linux)
    # Zsh
    if command -v zsh >/dev/null 2>&1; then
      if $use_system_paths; then
        ZSH_DIR="/usr/share/zsh/vendor-completions"
        mkdir -p "$ZSH_DIR" || true
        "$BIN" completions zsh > "$ZSH_DIR/_hot" || true
        echo "Installed Zsh completion to $ZSH_DIR/_hot" || true
      else
        ZSH_DIR="$HOME/.zsh/completions"
        mkdir -p "$ZSH_DIR"
        "$BIN" completions zsh > "$ZSH_DIR/_hot"
        echo "Installed Zsh completion to $ZSH_DIR/_hot"
      fi
    fi

    # Bash
    if command -v bash >/dev/null 2>&1; then
      if $use_system_paths; then
        if [ -d "/etc/bash_completion.d" ]; then
          "$BIN" completions bash > "/etc/bash_completion.d/hot" || true
          echo "Installed Bash completion to /etc/bash_completion.d/hot" || true
        fi
      else
        mkdir -p "$HOME/.bash_completion.d"
        "$BIN" completions bash > "$HOME/.bash_completion.d/hot"
        if ! grep -q "\.\s\+\$HOME/.bash_completion.d/hot" "$HOME/.bashrc" 2>/dev/null; then
          echo ". $HOME/.bash_completion.d/hot" >> "$HOME/.bashrc"
        fi
        echo "Installed Bash completion to $HOME/.bash_completion.d/hot (sourced from ~/.bashrc)"
      fi
    fi

    # Fish
    if command -v fish >/dev/null 2>&1; then
      if $use_system_paths; then
        for d in /usr/share/fish/vendor_completions.d /usr/local/share/fish/vendor_completions.d; do
          if [ -d "$d" ]; then
            "$BIN" completions fish > "$d/hot.fish" || true
            echo "Installed Fish completion to $d/hot.fish" || true
            break
          fi
        done
      else
        FISH_DIR="$HOME/.config/fish/completions"
        mkdir -p "$FISH_DIR"
        "$BIN" completions fish > "$FISH_DIR/hot.fish"
        echo "Installed Fish completion to $FISH_DIR/hot.fish"
      fi
    fi
    ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    # Windows / Git Bash environments
    if command -v pwsh >/dev/null 2>&1; then
      PS_PROFILE_DIR="$HOME/Documents/PowerShell"
      mkdir -p "$PS_PROFILE_DIR"
      "$BIN" completions powershell | iconv -t utf-8 > "$PS_PROFILE_DIR/Microsoft.PowerShell_profile.ps1"
      echo "Installed PowerShell completion, appended to $PS_PROFILE_DIR/Microsoft.PowerShell_profile.ps1"
    elif command -v powershell >/dev/null 2>&1; then
      PS_PROFILE_DIR="$HOME/Documents/WindowsPowerShell"
      mkdir -p "$PS_PROFILE_DIR"
      "$BIN" completions powershell | iconv -t utf-8 > "$PS_PROFILE_DIR/Microsoft.PowerShell_profile.ps1"
      echo "Installed PowerShell completion, appended to $PS_PROFILE_DIR/Microsoft.PowerShell_profile.ps1"
    fi

    if command -v bash >/dev/null 2>&1; then
      mkdir -p "$HOME/.bash_completion.d"
      "$BIN" completions bash > "$HOME/.bash_completion.d/hot"
      if ! grep -q "\.\s\+\$HOME/.bash_completion.d/hot" "$HOME/.bashrc" 2>/dev/null; then
        echo ". $HOME/.bash_completion.d/hot" >> "$HOME/.bashrc"
      fi
      echo "Installed Bash completion to $HOME/.bash_completion.d/hot (sourced from ~/.bashrc)"
    fi
    ;;
  *)
    echo "Unsupported platform: $OS" >&2
    exit 0
    ;;
esac

echo "Done. Restart your shell or source the appropriate file to enable completions."
