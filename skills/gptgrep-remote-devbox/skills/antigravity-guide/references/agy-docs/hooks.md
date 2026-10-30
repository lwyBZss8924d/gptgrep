# Hooks

Available on: Antigravity App, Antigravity CLI, Antigravity IDE

Hooks allow you to run custom scripts or shell commands at specific points during Antigravity’s execution loop to enforce rules, execute linters, or capture diagnostics.

## Antigravity App

### Managing hooks in the Antigravity App

In the Antigravity App, hooks are configured in a `hooks.json` file located in your customization directory:

*   **Workspace level**: `.agents/hooks.json` in your workspace root.
*   **Global level**: `~/.gemini/config/hooks.json`.

You can also view and toggle hooks from **Settings** > **Customizations** > **Hooks**. For the Antigravity App, `<app_data_dir>` in hook payloads resolves to `~/.gemini/antigravity`.

## Antigravity CLI

### Managing hooks in Antigravity CLI

Hooks intercept agent actions right before or immediately after execution. They are useful for running automated pre-flight checks or post-generation formatters (such as running `prettier` after writing files).

You can define hooks in any of the following locations:

*   **Workspace level**: `.agents/hooks.json` at your project root.
*   **Global level**: `~/.gemini/config/hooks.json` or inside your primary `~/.gemini/antigravity-cli/settings.json` file.
*   **Plugin level**: Packaged inside an installed plugin’s `hooks.json` file.

You can inspect all loaded and active hooks interactively inside the TUI by typing:

```text
/hooks
```

For Antigravity CLI, `<app_data_dir>` in hook payloads resolves to `~/.gemini/antigravity-cli`.

## Antigravity IDE

### Managing hooks in Antigravity IDE

In the standalone Antigravity IDE, hooks are configured in a `hooks.json` file:

*   **Workspace level**: `.agents/hooks.json` in your open project.
*   **Global level**: `~/.gemini/config/hooks.json`.

You can manage active hooks from the **…** > **Customizations** > **Hooks** menu in the agent side panel. For Antigravity IDE, `<app_data_dir>` in hook payloads resolves to `~/.gemini/antigravity-ide`.

## Schema and file format

The `hooks.json` file maps hook names to their event configurations:

```json
{
  "my-linter-hook": {
    "PostToolUse": [
      {
        "matcher": "run_command",
        "hooks": [
          {
            "type": "command",
            "command": "./scripts/lint.sh",
            "timeout": 10
          }
        ]
      }
    ]
  },
  "safety-gate": {
    "enabled": false,
    "PreToolUse": [
      {
        "matcher": "run_command",
        "hooks": [
          {
            "command": "./scripts/safety-check.sh"
          }
        ]
      }
    ]
  },
  "reminder": {
    "PreInvocation": [
      {
        "type": "command",
        "command": "./scripts/reminder.sh"
      }
    ]
  },
  "session-init": {
    "SessionStart": [
      {
        "type": "command",
        "command": "./scripts/init-env.sh"
      }
    ]
  }
}
```

### Hook definition fields

Each hook definition supports the following fields:

| Field | Type | Description |
| :-- | :-- | :-- |
| `enabled` | boolean | Optional. Set to `false` to disable the hook without removing it. Defaults to `true`. |
| `SessionStart` | array | Handlers that run once when a new conversation session starts, before the first model invocation. |
| `PreToolUse` | array | Handlers that run before a tool is executed. |
| `PostToolUse` | array | Handlers that run after a tool completes. |
| `PreInvocation` | array | Handlers that run before Antigravity calls the model. |
| `PostInvocation` | array | Handlers that run immediately after each model invocation completes. |
| `Stop` | array | Handlers that run when the execution loop terminates. |

## Supported events

Antigravity supports the following hook events:

| Event | Description | Matcher Target |
| :-- | :-- | :-- |
| `SessionStart` | Fires once when a new conversation session starts, before the first model invocation. | N/A (matcher ignored) |
| `PreToolUse` | Fires before a tool is executed. | Tool name (for example, `run_command`) |
| `PostToolUse` | Fires after a tool completes. | Tool name |
| `PreInvocation` | Fires before the model is called. | N/A (matcher ignored) |
| `PostInvocation` | Fires immediately after each model invocation completes. | N/A (matcher ignored) |
| `Stop` | Fires when execution terminates. | N/A (matcher ignored) |

### Matcher

For `PreToolUse` and `PostToolUse`, you can use a regular expression in the `matcher` field to specify which tools trigger the hook:

*   `""` or `"*"`: Match all tools.
*   `"run_command"`: Match exactly `run_command`.
*   `"run_command|view_file"`: Match either tool.
*   `"browser_.*"`: Match any tool starting with `browser_`.

Note

**Note**: For `SessionStart`, `PreInvocation`, `PostInvocation`, and `Stop`, the structure is simpler (a list of handlers directly under the event key) and the matcher is ignored.

## Supported tools

For `PreToolUse` and `PostToolUse` matchers, you can match against standard tool names, grouped by category.

### File and directory operations

The following tools manage files and directories:

*   **`view_file`**: View the contents of a file.
    *   **Arguments**: `AbsolutePath`, `StartLine` (optional), `EndLine` (optional), `IsSkillFile` (optional)
*   **`write_to_file`**: Create new files.
    *   **Arguments**: `TargetFile`, `Overwrite`, `CodeContent`, `Description`, `IsArtifact` (optional), `ArtifactMetadata` (optional)
*   **`replace_file_content`**: Edit a single contiguous block of text in a file.
    *   **Arguments**: `TargetFile`, `Instruction`, `Description`, `AllowMultiple`, `TargetContent`, `ReplacementContent`, `StartLine`, `EndLine`, `TargetLintErrorIds` (optional)
*   **`multi_replace_file_content`**: Make multiple, non-contiguous edits to the same file.
    *   **Arguments**: `TargetFile`, `Instruction`, `Description`, `ReplacementChunks` (array of chunks), `TargetLintErrorIds` (optional), `ArtifactMetadata` (optional)
*   **`list_dir`**: List the contents of a directory.
    *   **Arguments**: `DirectoryPath`
*   **`find_by_name`**: Search for files and directories using glob patterns.
    *   **Arguments**: `SearchDirectory`, `Pattern`, `Type` (optional), `Excludes` (optional), `Extensions` (optional), `FullPath` (optional), `MaxDepth` (optional)

### Search and research

The following tools search files and the web:

*   **`grep_search`**: Run fast text searches within specific paths.
    *   **Arguments**: `SearchPath`, `Query`, `IsRegex` (optional), `CaseInsensitive` (optional), `Includes` (optional), `MatchPerLine` (optional)
*   **`search_web`**: Perform a general web search.
    *   **Arguments**: `query`, `domain` (optional)
*   **`read_url_content`**: Fetch text content of a public URL.
    *   **Arguments**: `Url`

### System and execution

The following tools execute commands and manage permissions:

*   **`run_command`**: Propose a Bash command to run.
    *   **Arguments**: `CommandLine`, `Cwd`, `WaitMsBeforeAsync`, `RunPersistent` (optional), `RequestedTerminalID` (optional)
*   **`manage_task`**: Interact with background tasks.
    *   **Arguments**: `Action` (`'list'`, `'kill'`, `'status'`, `'send_input'`), `TaskId` (optional), `Input` (optional)
*   **`schedule`**: Set timers or recurring cron jobs.
    *   **Arguments**: `DurationSeconds` (optional), `CronExpression` (optional), `MaxIterations` (optional), `Prompt`
*   **`list_permissions`**: View current resource access grants.
    *   **Arguments**: None
*   **`ask_permission`**: Request additional scoped permissions.
    *   **Arguments**: `Action`, `Target`, `Reason`

### Agent collaboration

The following tools coordinate subagents:

*   **`invoke_subagent`**: Spawn specialized subagents.
    *   **Arguments**: `Subagents` (array of specs with `Prompt`, `Role`, `TypeName`, `Workspace` (optional))
*   **`define_subagent`**: Create a custom subagent.
    *   **Arguments**: `name`, `description`, `system_prompt`, `enable_mcp_tools` (optional), `enable_write_tools` (optional), `enable_subagent_tools` (optional)
*   **`send_message`**: Communicate with other agents.
    *   **Arguments**: `Recipient`, `Message`
*   **`manage_subagents`**: List or terminate active subagents.
    *   **Arguments**: `Action` (`'list'`, `'kill'`, `'kill_all'`), `ConversationIds` (optional)

### Interaction and media

The following tools handle user interaction and media generation:

*   **`ask_question`**: Ask multiple-choice questions.
    *   **Arguments**: `questions` (array of questions with `question`, `options`, `is_multi_select`)
*   **`generate_image`**: Create or edit images.
    *   **Arguments**: `Prompt`, `ImageName`, `ImagePaths` (optional)

## Hook handler configuration

Each item in the `hooks` array supports the following fields:

| Field | Type | Description |
| :-- | :-- | :-- |
| `type` | string | Optional. Currently only `"command"` is supported. Defaults to `"command"`. |
| `command` | string | Required. The shell command to execute. |
| `timeout` | integer | Optional. Timeout in seconds. Defaults to `30`. |

## Input and output contract

Hooks receive input through **stdin** as JSON and return output through **stdout** as JSON. Field names use camelCase.

### Common input fields

All hooks receive the following system metadata fields in their input payload on `stdin`:

| Field | Type | Description |
| :-- | :-- | :-- |
| `conversationId` | string | The unique UUID of the active agent conversation. |
| `workspacePaths` | array of strings | Absolute directory paths representing your mounted workspaces. |
| `transcriptPath` | string | The absolute path to the persistent `transcript.jsonl` conversation logs.  
**Note**: This file lives in `<app_data_dir>/brain/<conversationId>/.system_generated/logs/transcript.jsonl` where `<app_data_dir>` is:
*   `~/.gemini/antigravity` for the Antigravity App
*   `~/.gemini/antigravity-cli` for Antigravity CLI
*   `~/.gemini/antigravity-ide` for Antigravity IDE

 |
| `artifactDirectoryPath` | string | The absolute path to the directory containing all conversation artifacts and screenshots. |
| `modelName` | string | The name or identifier of the model handling the invocation (for example, `gemini-3.6-flash-medium`). |

* * *

### SessionStart

Fires once when a new conversation session starts, before the first model invocation.

**Input fields (stdin)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| _(Common fields)_ |  | Includes `conversationId`, `workspacePaths`, `transcriptPath`, `artifactDirectoryPath`, `modelName`. |

**Output fields (stdout)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| `injectSteps` | array of objects | **Optional.** List of steps to inject into the conversation trajectory at the start of the session (same schema as `PreInvocation` inject steps). |

**Example**:

*   **Input (stdin)**:

```json
{
  "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
  "workspacePaths": ["/workspace/project"],
  "transcriptPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587/.system_generated/logs/transcript.jsonl",
  "artifactDirectoryPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587",
  "modelName": "gemini-3.6-flash-medium"
}
```

*   **Output (stdout)**:

```json
{
  "injectSteps": [
    {
      "systemMessage": {
        "systemMessage": "Environment initialized for workspace /workspace/project"
      }
    }
  ]
}
```

* * *

### PreToolUse

Fires before a tool is executed.

**Input fields (stdin)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| `toolCall` | object | Details of the proposed tool call. |
| `toolCall.name` | string | The name of the tool being executed (for example, `run_command`). |
| `toolCall.args` | object | Arguments passed to the tool call. |
| `stepIdx` | integer | The 0-based index of the current step in the trajectory. |
| _(Common fields)_ |  | Includes `conversationId`, `workspacePaths`, `transcriptPath`, `artifactDirectoryPath`, `modelName`. |

**Output fields (stdout)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| `decision` | string | **Required.** Controls how the tool call is gated:  
\- `"allow"`: Allows the tool call to proceed, delegating to the next hook or default policy.  
\- `"auto_approve"`: Automatically approves the tool call when the default policy would otherwise prompt, while honoring explicit user-configured rules.  
\- `"deny"`: Hard blocks execution immediately.  
\- `"ask"`: Prompts you for approval, but respects “Always Allow” settings.  
\- `"force_ask"`: Always prompts you for approval, ignoring cached permissions.  
\- `"deny_unless_prior_grant"`: Denies execution unless the resource was previously approved in a prior grant. |
| `reason` | string | **Optional.** The explanation shown to the agent or to you for the decision. |
| `overwrite` | object | **Optional.** Replacement arguments merged (shallow top-level merge) into `toolCall.args` before execution. When arguments are modified, a notice naming the changed keys is prepended to the tool result. |
| `permissionOverrides` | array of strings | **Optional.** A list of resource strings (for example, `["read_file(/path)", "command(args)"]`) to override default tool permissions. |

**Example**:

*   **Input (stdin)**:

```json
{
  "toolCall": {
    "name": "run_command",
    "args": {
      "CommandLine": "npm test",
      "Cwd": "/workspace/project",
      "WaitMsBeforeAsync": 5000
    }
  },
  "stepIdx": 19,
  "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
  "workspacePaths": ["/workspace/project"],
  "transcriptPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587/.system_generated/logs/transcript.jsonl",
  "artifactDirectoryPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587",
  "modelName": "gemini-3.6-flash-medium"
}
```

*   **Output (stdout)**:

```json
{
  "decision": "ask",
  "reason": "Requires confirmation for test execution.",
  "permissionOverrides": ["command(npm test)"]
}
```

* * *

### PostToolUse

Fires after a tool completes.

**Input fields (stdin)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| `toolCall` | object | Details of the executed tool call (`name` and `args`). |
| `stepIdx` | integer | The 0-based index of the completed step. |
| `error` | string | Optional. The detailed runtime error message if the tool call failed. Empty if successful. |
| _(Common fields)_ |  | Includes `conversationId`, `workspacePaths`, `transcriptPath`, `artifactDirectoryPath`, `modelName`. |

**Output fields (stdout)**: Returns an empty JSON object `{}`.

**Example**:

*   **Input (stdin)**:

```json
{
  "toolCall": {
    "name": "run_command",
    "args": {
      "CommandLine": "npm test",
      "Cwd": "/workspace/project",
      "WaitMsBeforeAsync": 5000
    }
  },
  "stepIdx": 5,
  "error": "exit status 1",
  "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
  "workspacePaths": ["/workspace/project"],
  "transcriptPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587/.system_generated/logs/transcript.jsonl",
  "artifactDirectoryPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587",
  "modelName": "gemini-3.6-flash-medium"
}
```

*   **Output (stdout)**: `{}`

* * *

### PreInvocation

Fires before the model is called.

**Input fields (stdin)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| `invocationNum` | integer | The 0-indexed sequence number of the current model invocation (the first invocation is 0). |
| `initialNumSteps` | integer | The number of steps currently in the trajectory. |
| _(Common fields)_ |  | Includes `conversationId`, `workspacePaths`, `transcriptPath`, `artifactDirectoryPath`, `modelName`. |

**Output fields (stdout)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| `injectSteps` | array of objects | **Optional.** List of steps to inject into the conversation trajectory before the model is called. |

Each object in the `injectSteps` array can have one of the following fields:

*   **`systemMessage`** (object): An out-of-band system instruction for the agent (for example, `{"systemMessage": "Remember to lint", "metadata": {"source": "policy"}}`).
*   **`userMessage`** (string): A message from you.
*   **`ephemeralMessage`** (string): A transient system message.
*   **`toolCall`** (object): A tool call to execute.

**Example**:

*   **Input (stdin)**:

```json
{
  "invocationNum": 3,
  "initialNumSteps": 10,
  "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
  "workspacePaths": ["/workspace/project"],
  "transcriptPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587/.system_generated/logs/transcript.jsonl",
  "artifactDirectoryPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587",
  "modelName": "gemini-3.6-flash-medium"
}
```

*   **Output (stdout)**:

```json
{
  "injectSteps": [{"ephemeralMessage": "Remember to lint"}]
}
```

* * *

### PostInvocation

Fires immediately after each model invocation completes.

**Input fields (stdin)**: Same as `PreInvocation` input fields (`invocationNum` and `initialNumSteps`).

**Output fields (stdout)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| `injectSteps` | array of objects | **Optional.** List of steps to inject after the invocation completes (same schema as `PreInvocation` inject steps). |
| `terminationBehavior` | string | **Optional.** Controls the execution flow after injection:  
\- `"force_continue"`: Forces the loop to continue.  
\- `"terminate"`: Forces the loop to terminate.  
\- `""` (or omitted): Default behavior. |

**Example**:

*   **Input (stdin)**: Same as `PreInvocation`.
*   **Output (stdout)**:

```json
{
  "injectSteps": [],
  "terminationBehavior": ""
}
```

* * *

### Stop

Fires when the execution loop terminates.

**Input fields (stdin)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| `executionNum` | integer | The sequence number of the execution attempt. |
| `terminationReason` | string | The reason why the execution is stopping (for example, `"model_stop"`, `"max_steps_exceeded"`, `"error"`). |
| `error` | string | Optional. The error message if termination was caused by a system error. |
| `fullyIdle` | boolean | **Required.** `true` if the agent is completely finished and all background commands or asynchronous tasks have completed. `false` if active background tasks are still running. |
| _(Common fields)_ |  | Includes `conversationId`, `workspacePaths`, `transcriptPath`, `artifactDirectoryPath`, `modelName`. |

**Output fields (stdout)**:

| Field | Type | Description |
| :-- | :-- | :-- |
| `decision` | string | **Required.** Set to `"continue"` to prevent the agent from stopping and re-enter the execution loop. Any other value allows the stop. |
| `reason` | string | **Optional.** If `decision` is `"continue"`, this message is injected as a system message into the conversation. |

**Example**:

*   **Input (stdin)**:

```json
{
  "executionNum": 1,
  "terminationReason": "model_stop",
  "error": "",
  "fullyIdle": true,
  "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
  "workspacePaths": ["/workspace/project"],
  "transcriptPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587/.system_generated/logs/transcript.jsonl",
  "artifactDirectoryPath": "~/.gemini/antigravity/brain/ec33ebf9-0cba-4100-8142-c61503f6c587",
  "modelName": "gemini-3.6-flash-medium"
}
```

*   **Output (stdout)**:

```json
{
  "decision": "continue",
  "reason": "Not done yet"
}
```