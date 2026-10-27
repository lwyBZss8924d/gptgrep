#!/usr/bin/env bash
# devbox-bootstrap.sh: Idempotent remote initialization runner for gptgrep-remote-devbox
set -euo pipefail

MOUNT_POINT="${MOUNT_POINT:-/mnt/gptgrep-dev}"
AGENTS_HOME="${MOUNT_POINT}/agents-home"
TARGET_USER="${TARGET_USER:-$(id -un)}"
TARGET_GROUP="${TARGET_GROUP:-$(id -gn)}"

echo "=== Initializing DevBox Agents Home: ${AGENTS_HOME} ==="

if [ ! -d "${MOUNT_POINT}" ]; then
  echo "ERROR: ${MOUNT_POINT} is not mounted. Refusing ephemeral initialization." >&2
  exit 1
fi

# Scaffolding directories
mkdir -p "${AGENTS_HOME}"/{skills,references,configs,bin,clients/{claude,codex,gemini}}
mkdir -p "${MOUNT_POINT}"/docker-data

# Symlink user ~/.agents
if [ ! -L "${HOME}/.agents" ] || [ "$(readlink -f "${HOME}/.agents")" != "${AGENTS_HOME}" ]; then
  ln -sfn "${AGENTS_HOME}" "${HOME}/.agents"
  echo "Linked ${HOME}/.agents -> ${AGENTS_HOME}"
fi

# Client persistent state symlinks
for client in claude codex gemini; do
  mkdir -p "${AGENTS_HOME}/clients/${client}"
  if [ ! -L "${HOME}/.${client}" ]; then
    ln -sfn "${AGENTS_HOME}/clients/${client}" "${HOME}/.${client}"
    echo "Linked ${HOME}/.${client} -> ${AGENTS_HOME}/clients/${client}"
  fi
done

# Client autoload symlinks
mkdir -p "${HOME}/.claude" "${HOME}/.codex" "${HOME}/.gemini/config"
ln -sfn "${HOME}/.agents/AGENTS.md" "${HOME}/.claude/CLAUDE.md"
ln -sfn "${HOME}/.agents/skills" "${HOME}/.claude/skills"

ln -sfn "${HOME}/.agents/AGENTS.md" "${HOME}/.codex/AGENTS.md"
ln -sfn "${HOME}/.agents/skills" "${HOME}/.codex/skills"

ln -sfn "${HOME}/.agents/AGENTS.md" "${HOME}/.gemini/AGENTS.md"
ln -sfn "${HOME}/.agents/AGENTS.md" "${HOME}/.gemini/GEMINI.md"
ln -sfn "${HOME}/.agents/skills" "${HOME}/.gemini/config/skills"

echo "=== DevBox Agents Home bootstrap complete ==="
