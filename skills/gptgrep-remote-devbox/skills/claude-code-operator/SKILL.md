---
name: claude-code-operator
description: >-
  Operator skill and guidance for invoking, configuring, and maintaining Anthropic Claude Code (claude)
  in non-interactive, headless, and autonomous remote DevBox environments with persistent state,
  session continuity, and permission-bypass execution.
---

# Claude Code (`claude`) Operator Skill

This skill provides authoritative operational guidance for running **Anthropic Claude Code (`claude`)** autonomously on the Remote DevBox (`gptgrep-devbox-01`) and local development environments.

## DevBox Invocation Contract

### Autonomous & Non-Interactive Invocations
To run Claude Code non-interactively without prompting for tool confirmation:
```bash
claude --dangerously-skip-permissions --resume
```

For one-shot execution with a prompt:
```bash
claude --dangerously-skip-permissions -p "Your prompt here"
```

### Key CLI Flags
- `--dangerously-skip-permissions`: Bypasses all confirmation dialogs for file modifications and terminal commands (equivalent to `--permission-mode bypassPermissions`).
- `--resume`, `-r`: Resumes the most recent session in the working directory, preserving conversation memory and context across invocations.
- `--print`, `-p`: Non-interactive one-shot query execution. Output is streamed directly to stdout.
- `--permission-mode <mode>`: Sets tool approval mode (`default`, `planOnly`, `bypassPermissions`).
- `--verbose`: Enables debug telemetry and detailed logging to stderr.

## Persistent Storage & Symlink Wiring

To preserve sessions, credentials, and settings across GCE VM suspend/resume cycles, Claude Code state is persisted on the dedicated 250 GB SSD volume (`/mnt/gptgrep-dev`):

### Directory Layout
- **Persistent Storage**: `/mnt/gptgrep-dev/agents-home/clients/claude`
- **Home Symlink**: `~/.claude -> /mnt/gptgrep-dev/agents-home/clients/claude`

### Autoload Wiring Matrix
To ensure Claude Code automatically inherits global repository governance and shared skills:
1. `~/.claude/CLAUDE.md -> ~/.agents/AGENTS.md`
2. `~/.claude/skills -> ~/.agents/skills`

## Authentication & Token Setup

### Non-Interactive Setup Token Flow
For automated environments where OAuth browser redirection is unavailable:
```bash
claude setup-token
```
This prints a long-lived machine token suitable for saving in `~/.claude/config.json` or passing via environment variables.

### Environment Credentials
Claude Code recognizes:
- `ANTHROPIC_API_KEY`: Direct API key for Claude 3.5 / 3.7 / Opus / Sonnet models.
- `CLAUDE_CONFIG_DIR`: Override default config directory path (defaults to `~/.claude`).

## Session Management & Continuity

### Resuming Conversations
```bash
# Resume latest session in current directory
claude --resume

# List available session histories
claude --resume --list
```

### Checkpointing & Memory
Claude Code maintains conversation checkpoints in `~/.claude/sessions/`. Memory files (`MEMORY.md`) provide persistent cross-session knowledge storage within the project workspace.

## Reference Documentation

Comprehensive offline reference documentation snapshots are bundled in `references/claude-code-docs/`:
- `cli-reference.md`: Complete command line reference and flags
- `authentication.md`: Auth mechanisms, tokens, and OAuth details
- `permission-modes.md`: Detailed security permissions model
- `settings-reference.md`: Configuration schema for `config.json`
- `checkpointing.md`: Checkpoint recovery and rollback mechanics
- `mcp.md`: Model Context Protocol server configuration
- `hooks.md`: Pre-tool and post-tool lifecycle execution hooks
