# CLI troubleshooting

Diagnose and resolve common anomalies with installation PATHs, local self-updating locks, keyring access permissions, and SSH clipboard forwarding.

## Quick reference

Scan the following lookup table to identify symptoms and access immediate solutions:

| Error Symptom | Potential Cause | Target Resolution |
| :-- | :-- | :-- |
| **`agy: command not found`** | Binary directory missing from shell environments. | [Configure your shell PATH](#configure-your-shell-path) |
| **`keyring: secure lock out`** | Missing system service permissions or active lockouts. | [Authorize keyring permissions](#authorize-keyring-permissions) |
| **`SSH Clipboard paste failures`** | Protocol streams blocked or missing forward configurations. | [Enable emulator clipboard forwarding](#enable-emulator-clipboard-forwarding) |
| **`Advisory lock / update failures`** | Locked self-updater thread or read-only directory paths. | [Resolve self-updater locks and failures](#resolve-self-updater-locks-and-failures) |

* * *

## Configure your shell PATH

### Symptom

Executing `agy` returns a shell terminal error:

```bash
bash: agy: command not found
```

### Cause

The installation utility downloads the binary to `~/.local/bin` (or `C:\Users\<username>\AppData\Local\agy\bin`), but your shell’s active `$PATH` environment doesn’t index this directory.

### Resolution

Ensure your terminal session loads the binary path.

**macOS and Linux**:

To add the binary directory to your path on macOS or Linux, follow these steps:

1.  Open your shell configuration file (`~/.bashrc` or `~/.zshrc`).
2.  Verify or append the following line at the end of the file:
    
    ```bash
    export PATH="~/.local/bin:$PATH"
    ```
    
3.  Reload your profile configurations:
    
    ```bash
    source ~/.zshrc
    ```
    

**Windows (PowerShell)**:

To add the binary directory to your path on Windows, follow these steps:

1.  Open a PowerShell terminal as an Administrator and execute the following command:
    
    ```powershell
    [System.Environment]::SetEnvironmentVariable("Path", [System.Environment]::GetEnvironmentVariable("Path", "User") + ";C:\Program Files\Google\antigravity-cli", "User")
    ```
    
2.  Restart your terminal emulator for the system registry environment to refresh.

* * *

## Authorize keyring permissions

### Symptom

When launching, the CLI hangs, prints D-Bus warnings, or throws keyring access exceptions:

```text
Error: failed to retrieve token: secret keyring is locked
```

### Cause

Antigravity CLI uses secure keychain libraries (Apple Keychain, Linux secret-service through D-Bus, or Windows Credential Manager) to encrypt your session tokens. If the background daemon is locked or headless, the CLI can’t read credentials.

### Resolution

**macOS**:

To authorize keychain access on macOS, follow these steps:

1.  Open the **Keychain Access** app.
2.  Search for the `Antigravity CLI` security item.
3.  Right-click, select **Get Info**, choose the **Access Control** tab, and verify that `agy` is on the allowed applications list.
4.  If you’re running inside a headless SSH session on macOS, enforce an auto-lock inactivity timeout (for example, 5 minutes) and unlock the keychain interactively:
    
    ```bash
    security set-keychain-settings -l -u -t 300 ~/Library/Keychains/login.keychain-db
    security unlock-keychain ~/Library/Keychains/login.keychain-db
    ```
    
    When your session is finished, optionally re-lock the keychain immediately:
    
    ```bash
    security lock-keychain ~/Library/Keychains/login.keychain-db
    ```
    

**Linux**:

Ensure your system keyring (such as GNOME Keyring or KWallet) is unlocked and accessible.

If you’re running in a headless environment or over SSH, ensure that a D-Bus session is active and that your keyring daemon is running. You can typically initialize a D-Bus session by running:

```bash
export $(dbus-launch)
```

If you still experience access issues, ensure your user account has the necessary permissions to access the keyring service or reach out to support.

* * *

## Enable emulator clipboard forwarding

### Symptom

Pasting screenshots or media files using `Ctrl+V` within an SSH terminal returns a failure notification:

```text
Error: local pasteboard is empty or unreachable over SSH connection
```

### Cause

Standard SSH streams don’t forward graphical clipboards. Graphic uploads require specific terminal multiplexer protocols.

### Resolution

Verify that you’re using supported terminal emulators and configurations:

1.  **Use iTerm2 or Ghostty**: These emulators support advanced clip channels.
2.  **Configure iTerm2 forwarding**: Enable clipboard access in your terminal settings:
    *   Open iTerm2 Preferences (`Cmd+,`).
    *   Go to **General** > **Selection**.
    *   Select **Applications in terminal may access clipboard** (enabling OSC 52 write channels).
3.  **Bypass multiplexers**: If running inside `tmux`, ensure your active configuration maps standard paste clips:
    
    ```text
    set -s set-clipboard on
    ```
    

* * *

## Resolve self-updater locks and failures

### Symptom

Launching `agy` hangs, fails to apply upgrades, or returns an advisory lock warning:

```text
Warning: another background updater process is already active (update.lock)
```

### Cause

Antigravity CLI contains a native, statically linked self-updater that runs in the background. It uses a 15-minute Time-To-Live (TTL) debounce marker (`last_check.timestamp`) and an advisory lock (`update.lock`) inside `~/.gemini/antigravity-cli/updater/` to prevent concurrent process collisions. If a background updater process hangs, crashes without releasing the lock, or has insufficient user filesystem permissions inside the executable directory, subsequent updates are blocked.

### Resolution

Try any of the following steps to resolve the updater lock:

*   **Release the advisory lock**: Purge the background lock file manually:
    
    ```bash
    rm -f ~/.gemini/antigravity-cli/updater/update.lock
    ```
    
*   **Opt out or disable auto-updates**: Set the `AGY_CLI_DISABLE_AUTO_UPDATE` environment variable to `true` inside your shell profile (`~/.bashrc` or `~/.zshrc`):
    
    ```bash
    export AGY_CLI_DISABLE_AUTO_UPDATE=true
    ```
    
*   **Verify directory write permissions**: Ensure your user profile owns and has write permissions inside the target installation directory (`~/.local/bin/` on Unix, or `%LOCALAPPDATA%\agy\bin` on Windows).

* * *

## Next steps

Access the quick reference sheets or configure advanced permissions:

*   **[CLI reference](/docs/cli/reference)**: Reference tables listing all slash commands and visual settings keys.
*   **[Permissions](/docs/cli/permissions)**: Configure fine-grained allowed and denied action policies.
*   **[Sandbox](/docs/cli/sandbox)**: Enforce OS-level container isolation boundaries.
*   **[Plugins and skills](/docs/cli/plugins)**: Create your own custom skills.