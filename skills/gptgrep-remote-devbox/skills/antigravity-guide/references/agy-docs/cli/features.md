# Antigravity CLI features

### Plugins

**How plugins work** Plugins are namespaced bundles that can contain skills, agents, rules, MCP servers, and hooks as a single deployable unit.

When you install a plugin, the CLI stages the files in your home directory under `~/.gemini/antigravity-cli/plugins/<plugin_name>/`. The Antigravity agent automatically discovers and loads these staged customizations.

```plaintext
~/.gemini/antigravity-cli/
├── plugins/
│   └── <plugin_name>/
│       ├── plugin.json         # Required marker file
│       ├── mcp_config.json     # Optional MCP server definitions
│       ├── hooks.json          # Optional event hooks definition
│       ├── skills/             # Optional skills
│       ├── agents/             # Optional subagents
│       └── rules/              # Optional rules
└── import_manifest.json        # Tracking manifest
```

### Manage plugins with `/plugin` (Marketplace and installed)

Trigger the interactive Plugins Manager in the CLI using the `/plugin` slash command (alias `/plugins`), and press Tab to switch between the **Installed** tab and the **Discover** tab:

*   **Discover tab**: Browse and install available plugins from the catalog:
    
    ![Plugin discover tab](/assets/image/docs/plugins/plugin-discover-tab.png)
    
    *   Type to start searching for specific plugins.
    *   Press Enter to select **Install from local directory** and provide a local path for plugin installation.
    *   Press Enter on a plugin to expand its details view (includes description, components such as MCP and skills, marketplace, and version).
    *   Press Ctrl + S on an uninstalled plugin to install it (or Ctrl + S on an installed plugin to uninstall it).
    *   **After installation**: A green dot (`●`) appears beside the plugin name in the **Discover** tab denoting its installed status, and the plugin name appears in the **Installed** tab.
*   **Installed tab**: View and configure your installed plugins:
    
    ![Plugin installed tab](/assets/image/docs/plugins/plugin-install-tab.png)
    
    *   Type to start searching for installed plugins.
    *   Press Space to toggle enabling or disabling a plugin.
    *   Press Enter on an installed plugin to expand its details view (includes description, skills if applicable, MCP if applicable, marketplace, local installation path, and version).
*   **Inline commands**: Manage plugins directly from the prompt without opening the interactive panel:
    
    *   `/plugin` provides inline `install`, `uninstall`, `enable`, `disable`, and `list` subcommands without needing to open the interactive Plugins Manager panel.
    *   To install inline, run `/plugin install <plugin-name>@<marketplace-name>` (`<marketplace-name>` supports the official marketplace, `antigravity-plugins-official`) or `/plugin install <local-path>`. For example, running `/plugin install firebase@antigravity-plugins-official` outputs:
        
        ```text
        Successfully installed plugin "firebase" from "antigravity-plugins-official".
        ```
        

### Manage plugins from your shell (`agy plugin`)

Outside an interactive TUI session, you can install and manage plugins from your terminal using `agy plugin`:

*   **Install from the official marketplace**: Specify `<plugin-name>@antigravity-plugins-official` (`<marketplace-name>` only supports the official marketplace, `antigravity-plugins-official`), or pass a bare `<plugin-name>`, which defaults to `<plugin-name>@antigravity-plugins-official`:
    
    ```bash
    agy plugin install <plugin-name>@antigravity-plugins-official
    agy plugin install <plugin-name>
    ```
    
*   **Install from a GitHub link**: Clone and stage a plugin directly from a GitHub repository URL:
    
    ```bash
    agy plugin install https://github.com/<owner>/<repo>
    ```
    
*   **Install from a local directory**: Stage a local plugin package into your profile:
    
    ```bash
    agy plugin install </path/to/local/plugin>
    ```
    
*   **List installed plugins**: Run `agy plugin list` for a human-readable summary of installed plugins and their component counts, or pass `--json` to output a JSON array with installation paths and component names:
    
    ```bash
    agy plugin list
    agy plugin list --json
    ```
    
*   **Enable, disable, or uninstall plugins**:
    
    ```bash
    agy plugin enable <plugin-name>
    agy plugin disable <plugin-name>
    agy plugin uninstall <plugin-name>
    ```
    

Cross-surface synchronization

Plugins installed in the Antigravity App are automatically updated and shown in the CLI’s **Installed** tab. For more details, refer to the [Marketplace guide](/docs/marketplace?tab=cli).

**Accessing plugin components** Once staged and loaded, you can interact with the plugin components inside the CLI using slash commands.

### Terminal sandbox

The terminal sandbox is a lightweight security isolation mechanism that protects your host system from potentially destructive file manipulations or unauthorized outbound network requests when the agent executes local shell commands.

Rather than running heavy virtual machines or containers, the CLI uses native operating system features (`nsjail` on Linux, `sandbox-exec` on macOS, and `AppContainer` on Windows) to enforce strict containment boundaries with zero startup overhead.

**Configuration** You can configure the sandbox behavior in your `settings.json` file (located at `~/.gemini/antigravity-cli/settings.json`):

```json
{
    "enableTerminalSandbox": true
}
```

The configuration supports the following option:

*   **`enableTerminalSandbox`** (boolean, default: `false`): Enables general execution containment barriers on all local agent processes.

**Interactive approvals** When the agent proposes a terminal command that requires your confirmation, the CLI prompt adapts dynamically based on your settings:

*   **When the sandbox is enabled**: The confirmation prompt includes a specific option to **Yes, and run without sandbox restrictions** if you need to temporarily bypass the containment boundary for a single trusted command.
*   **When the sandbox is disabled**: The prompt includes an option to **Yes, and run in sandbox** if you want to force a specific, potentially risky command to execute within the safety boundary.

### CLI slash commands reference

The Antigravity CLI supports a variety of slash commands typed directly into the prompt box to manage conversations, configure settings, and inspect agent capabilities.

### Core slash commands

| Command | Category | Purpose |
| :-- | :-- | :-- |
| **[`/resume`](/docs/cli/commands/resume)** _(aliases `/switch`, `/conversation`)_ | Conversation | Open the conversation picker to resume or switch sessions. |
| **[`/boost <task>`](/docs/boost)** | Reasoning | Multi-agent deep reasoning for complex bugs, race conditions, and algorithms. |
| **[`/teamwork-preview <task>`](/docs/teamwork)** | Reasoning | Launch [collaborative multi-agent teams](/docs/teamwork) for long-horizon projects (paid plans). |
| **[`/effort [level]`](/docs/cli/commands/effort)** | Configuration | Set the active model’s [reasoning effort level](/docs/cli/commands/effort) (`low` through `max`, depending on the model). |
| **`/rewind`** _(alias `/undo`)_ | Conversation | Rewind conversation to a previous message. |
| **`/rename <name>`** | Conversation | Rename the active conversation thread for easier tracking. |
| **`/config`** _(alias `/settings`)_ | Configuration | Open the settings panel to configure CLI preferences and global permission modes (`request-review`, `always-proceed`, or `strict`). |
| **[`/permissions`](/docs/cli/commands/permissions)** | Configuration | Manage tool permissions (`allow`, `deny`, and `ask` rules across project, shared, and global scopes). |
| **`/model`** | Configuration | Set a model, or run a single prompt on another model. |
| **`/keybindings`** | Configuration | Open the interactive keyboard shortcut editor. |
| **[`/statusline`](/docs/cli/commands/statusline)** | Configuration | Toggle the statusline or configure a custom statusline command. |
| **`/tasks`** | Tools & Monitoring | Monitor, view logs for, or terminate active background tasks. |
| **[`/plugin`](/docs/plugins?tab=cli)** _(alias `/plugins`)_ | Tools & Monitoring | Open the interactive [Plugins Manager](/docs/marketplace?tab=cli) to discover, install, enable, disable, or uninstall plugins. |
| **`/skills`** | Tools & Monitoring | List available skills or reload skills from disk (`/skills reload`). |
| **`/mcp`** | Tools & Monitoring | Open the panel to configure and manage Model Context Protocol servers. |
| **[`/btw <query>`](/docs/cli/commands/btw)** | Utility | Ask a [side question in the background](/docs/cli/commands/btw) without interrupting the active conversation. |
| **[`/codesearch <query>`](/docs/cli/commands/codesearch)** _(aliases `/cs`, `/search`)_ | Utility | Open the [interactive Code Search panel](/docs/cli/commands/codesearch) to search code across the workspace. |
| **[`/context`](/docs/cli/commands/context)** | Utility | Visualize [active context window usage](/docs/cli/commands/context), category token breakdowns, and checkpoints. |
| **`/open <path>`** | Utility | Immediately open a file in your preferred external editor. |
| **[`/diff`](/docs/cli/commands/diff)** | Utility | Open the [interactive diff viewer](/docs/cli/commands/diff) to review changes and steer the agent. |
| **[`/remote-control`](/docs/remote-control?tab=cli#interactive-mode)** | Utility | Turn on or off Remote Control for the active terminal session. |
| **[`/changelog`](/docs/cli/commands/changelog)** | Utility | View [versioned CLI release notes](/docs/cli/commands/changelog) in the TUI or headless mode. |
| **`/help`** | Utility | Show available commands and keybindings. |
| **[`/usage`](/docs/cli/commands/usage)** _(alias `/quota`)_ | Utility | View model quota usage. |
| **`/logout`** | Account | Log out of your Google session and clear cached credentials. |

### Advanced customization using `settings.json`

For power users, several slash commands support deep customization using your `~/.gemini/antigravity-cli/settings.json` configuration:

*   **Fine-grained permissions**: Instead of global levels, define specific allowed or denied commands:
    
    ```json
    "permissions": {
      "allow": ["command(git)", "command(npm test)"],
      "deny": ["command(rm -rf)"]
    }
    ```
    
*   **Custom status line and window titles**: You can pipe live agent metadata (JSON format containing the current working directory, active model, token usage, state, and more) directly into your own custom shell scripts to generate dynamic status bars or terminal window titles.

### Subagents in Antigravity CLI

Antigravity CLI features an asynchronous subagents framework that allows the main agent to delegate parallel work, perform background research, and run system tests without blocking your active conversation.

**What are subagents?** Subagents are independent, concurrent agent sessions designed to tackle specific background tasks in parallel with the main conversation:

*   **Purpose**: The main agent automatically spawns subagents to perform background operations such as looking up documentation, running builds, or validating a fix.
*   **Capabilities**: Subagents have full access to tools such as code search, file editing, terminal commands, and web searches to complete their assigned tasks. The main agent decides what tools and permissions subagents get, including whether they can use MCP tools and whether they can write files.

### Managing agents: the `/agents` panel

Antigravity CLI provides an interactive terminal UI to view, manage, and approve actions for running subagents:

*   **Access**: Type `/agents` in the prompt to open the subagents panel.
*   **Overview**: The panel shows a list of active and completed subagents, including surface-level details such as their status (such as running, done, or killed) and the current step they’re executing.

Note

Selecting a subagent from the panel opens a full-screen detail view. This view shows the entirety of the subagent’s conversation, including its steps, thoughts, and tool execution logs.

**Tool confirmations and approvals** When a subagent needs to execute a tool that requires your permission (such as running a local command or writing a file), it surfaces the request. You can manage approvals in two ways:

1.  **Detail view approvals**: The subagent detail view features an interaction section containing all pending approvals, where you can selectively approve or deny requests.

Note

**Tip**: Use the keyboard shortcut `alt+j` to jump from the main conversation directly to the detailed view of the next subagent waiting for your approval.

2.  **Fast-path alerts**: To keep you in your flow, Antigravity CLI displays a fast-path alert directly above your prompt box when a subagent requests permission.

Note

**Tip**: You can approve a pending subagent permission instantly using `ctrl+k` without ever having to switch away from the main conversation.