# CLI settings and tips

### Settings

Antigravity CLI provides a flexible configuration system to customize workspace behavior, safety restrictions, editor preferences, visual style, and performance:

*   **Configuration file**: Stored in a plain JSON file at `~/.gemini/antigravity-cli/settings.json`.
*   **Settings panel**: Type `/config` or `/settings` to open a full-screen overlay menu listing all available options:
    *   Select a setting to open its list of options or a text input field.
    *   Immediately save your selection to the disk and return to the main list.
*   **Overrides**: Certain settings can be overridden at launch using CLI flags (such as `--sandbox` or `--dangerously-skip-permissions`):
    *   The settings menu displays an indicator showing where the override came from (for example, _Sandbox Mode on overridden by `--sandbox`_).
    *   You can still edit the persistent setting on disk, but the current session enforces the command-line override until restarted.

### Quick tips

The following table summarizes common shortcuts and commands:

| Action/Feature | Tip/Command |
| :-- | :-- |
| **Auto-complete to file paths** | `@` triggers path suggestions |
| **Clear prompt** | Type `esc esc` to clear your prompt box (when no streaming is active) |
| **Terminal commands** | Use `!` at the start of your prompt to run terminal commands directly |
| **Help** | Type `?` to get help and list all slash commands |
| **Reduce noise from tool calls** | Set verbosity to **low** in `/config` to minimize outputs from numerous tool calls |
| **Manage permissions** | Control permissions using `/config` or `/permissions` |
| **Go back in conversation** | Use `/rewind` or `/undo` to rewind the conversation history |
| **Fork conversation** | Use `/fork` to spin up a separate workspace and branch the conversation from an earlier point |
| **Clear conversation** | Use `/clear` to clear the prompt and start a new conversation session |
| **Resume conversation** | Use `/resume` to list and resume previous conversation logs |
| **Auto-save resume** | When you close the CLI, it automatically prints the exact command needed to resume that specific session |

### Keybindings

Antigravity CLI supports custom keybindings. You can edit them by typing `/keybindings` or modifying the JSON file directly:

*   **File location**: `~/.gemini/antigravity-cli/keybindings.json`.
*   **Reset**: To reset to default, delete the `keybindings.json` file.

**Default keybindings**

| Action/Command | Keys | Purpose |
| :-- | :-- | :-- |
| **Clear TUI screen** | `ctrl+l` | Clear terminal output |
| **Enter / submit** | `enter` | Submit prompts or choices |
| **Escape / cancel** | `ctrl+c`, `esc` | Stop stream, close menus, or clear prompt |
| **Exit CLI** | `ctrl+d` | Terminate CLI TUI session |
| **Suspend CLI** | `ctrl+z` | Push CLI session to terminal background |
| **Edit command** | `e` | Open editor to edit proposed terminal command |
| **Confirm no** | `n` | Decline terminal command execution |
| **Confirm yes** | `y` | Approve terminal command execution |
| **Open editor** | `ctrl+g` | Edit prompt inside your default shell editor |
| **Paste text** | `ctrl+v` | Paste text from your clipboard |
| **Redo text edit** | `ctrl+shift+z` | Redo last undone text change |
| **Undo text edit** | `ctrl+_`, `ctrl+shift+-` | Undo last text change |
| **Yank (copy)** | `ctrl+y` | Yank/copy selected text |
| **Navigate down** | `down` | Scroll down in menu lists |
| **Go to bottom** | `ctrl+end` | Jump TUI view directly to the bottom |
| **Go to top** | `ctrl+home` | Jump TUI view directly to the top |
| **Navigate left** | `left` | Move prompt cursor left |
| **Page down** | `pgdown`, `shift+down` | Scroll TUI page down |
| **Page up** | `pgup`, `shift+up` | Scroll TUI page up |
| **Navigate right** | `right` | Move prompt cursor right |
| **Tab / focus** | `tab` | Auto-complete choices or switch component focus |
| **Navigate up** | `up` | Scroll up in menu lists |
| **Insert newline** | `alt+enter`, `ctrl+j`, `shift+enter` | Add newline to prompt without submitting |

You can map a single action to many keybindings in the JSON file. To disable keybindings, set the list to empty (for example, `[]`). If the file is malformed, the CLI uses the valid parts and falls back to defaults for the broken actions.

Note

**Important**: Keybindings `cli.exit` and `cli.enter` can’t be disabled.