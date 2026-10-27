#!/usr/bin/env bash
# devbox-auth-helper.sh: Standalone AI client authentication manager for DevBox
set -euo pipefail

SUBCOMMAND="${1:-status}"

check_status() {
  echo "=== AI Engineer CLI Authentication Status ==="
  
  # Claude Code
  if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    echo "Claude Code: Authenticated via ANTHROPIC_API_KEY in environment"
  elif [ -f "${HOME}/.claude/config.json" ] || [ -f "${HOME}/.claude/.credentials.json" ]; then
    echo "Claude Code: Persistent credentials found on disk"
  else
    echo "Claude Code: No active credential detected (run: devbox-auth-helper claude-login)"
  fi

  # OpenAI Codex
  if [ -n "${OPENAI_API_KEY:-}" ]; then
    echo "OpenAI Codex: Authenticated via OPENAI_API_KEY in environment"
  elif [ -f "${HOME}/.codex/auth.json" ] || [ -f "/mnt/gptgrep-dev/development-artifacts/owner-private/codex-auth/canonical-home/auth.json" ]; then
    echo "OpenAI Codex: Canonical auth.json detected under lease broker"
  else
    echo "OpenAI Codex: No active credential detected (run: devbox-auth-helper codex-login)"
  fi

  # Google Antigravity
  if [ -n "${GEMINI_API_KEY:-}" ]; then
    echo "Google Antigravity: Authenticated via GEMINI_API_KEY in environment"
  elif [ -f "${HOME}/.gemini/antigravity-cli/auth.json" ] || [ -f "${HOME}/.gemini/jetski-standalone-oauth-token" ]; then
    echo "Google Antigravity: OAuth token found on disk"
  else
    echo "Google Antigravity: No active credential detected (run: devbox-auth-helper agy-login)"
  fi
}

case "${SUBCOMMAND}" in
  status)
    check_status
    ;;
  claude-login)
    echo "Launching Claude Code login..."
    claude login || claude auth login
    ;;
  codex-login)
    echo "Launching OpenAI Codex device-auth login..."
    codex login --device-auth
    ;;
  agy-login)
    echo "Launching Google Antigravity login..."
    agy auth login || agy login
    ;;
  *)
    echo "Usage: $0 [status|claude-login|codex-login|agy-login]" >&2
    exit 1
    ;;
esac
