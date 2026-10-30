# Sidecars

Available on: Antigravity App

Sidecars are background processes that run alongside Antigravity. Antigravity manages the lifecycle of sidecars, automatically launching them and restarting them if they crash or error.  
They are useful for persistent background scripts, scheduled recurring tasks, and reacting to events.

## Configuration

Antigravity discovers sidecars by searching for `sidecar.json` configuration files in two locations:

*   **Global sidecars**: Under `~/.gemini/config/sidecars/`
*   **Plugin sidecars**: Under `~/.gemini/config/plugins/<pluginName>/sidecars/`

Each sidecar has its own directory, and the directory name is used as the sidecar’s ID. Sidecars loaded from plugins have the ID `<pluginName>/<sidecarName>`.

The sidecar’s directory must contain a `sidecar.json` file and may also contain other helper files such as scripts to run. The sidecar’s directory also acts as the current working directory for the sidecar’s command.

The following example illustrates the directory structure:

```text
~/.gemini/config/sidecars/
├── sidecar1/
│   ├── sidecar.json
│   └── script.py
└── sidecar2/
    └── sidecar.json

~/.gemini/config/plugins/
└── my-plugin/
      └── sidecars/
            └── plugin-sidecar/
                  └── sidecar.json
```

### Config schema (`sidecar.json`)

The `sidecar.json` file supports the following configuration fields:

*   **`command`** (string): Command or executable (for example, `python3` or `/bin/bash`). Mutually exclusive with `builtin`.
*   **`builtin`** (string): Built-in command to execute. Currently supports `schedule`. Mutually exclusive with `command`.
*   **`args`** (array of strings): Optional. Arguments passed to the command or built-in function.
*   **`restart_policy`** (string): Optional. Restart behavior. One of `always`, `on-failure`, or `never`. Defaults to `always`.
*   **`description`** (string): Optional. Human-readable description of what the sidecar does.
*   **`env`** (object): Optional. Map of environment variables to set for the sidecar process.
*   **`display_name`** (string): Optional. Display name used in the UI.

One of `command` or `builtin` must be set.

The following examples demonstrate `sidecar.json` configurations:

```json
{
  "description": "Background worker",
  "command": "python3",
  "args": ["worker.py"],
  "restart_policy": "on-failure"
}
```

```json
{
  "description": "Hourly agent to triage review requests.",
  "builtin": "schedule",
  "args": [
    "0 * * * *",
    "agentapi",
    "new-conversation",
    "Give me a summary of incoming review requests."
  ]
}
```

### User configuration (`config.json`)

Sidecars are disabled unless you explicitly enable them in the global configuration file at `~/.gemini/config/config.json`:

*   **`enabled`** (boolean): Whether the sidecar is enabled.
*   **`projectId`** (string): Optional. The ID of the project where `agentapi` creates conversations.

The following example enables both a global sidecar and a plugin sidecar:

```json
{
  "sidecars": {
    "sidecar1": {
      "enabled": true
    },
    "my-plugin/plugin-sidecar": {
      "enabled": true,
      "projectId": "<projectId>"
    }
  }
}
```

### Runtime data

Runtime data produced by sidecars is stored in `~/.gemini/antigravity/sidecar_data/<sidecarId>/`.

This directory includes the following subdirectories:

*   **`data/`**: Subdirectory for any persistent data. This path is available through the `ANTIGRAVITY_EXECUTABLE_DATA_DIR` environment variable.
*   **`logs/`**: Auto-generated timestamped logs from stdout and stderr.
*   **`events/`**: JSON files recorded for `agentapi` calls.

### `schedule` builtin

`schedule` is a built-in scheduler for running recurring commands:

```json
{
  "builtin": "schedule",
  "args": [
    "* * * * *",
    "<command>",
    "<arg1>",
    "<arg2>"
  ]
}
```

The first argument is a standard five-field cron expression. The remaining arguments are the command and arguments to run on the specified schedule.

### `agentapi`

Sidecars can use the `agentapi` CLI to programmatically interact with Antigravity. The executable is automatically added to the sidecar’s path and available as `agentapi` with the following subcommands:

*   **`agentapi new-conversation <prompt>`**: Creates a new conversation. Sidecars creating conversations must have a `projectId` set.
*   **`agentapi send-message <conversation_id> <prompt>`**: Sends a message to an existing conversation.