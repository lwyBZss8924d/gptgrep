# Projects

Available on: Antigravity App, Antigravity CLI

Projects organize multi-folder workspace configurations, isolated agent settings, and conversation histories across Antigravity surfaces.

## Antigravity App

### What is a project?

A **project** is a configuration of folders defining the environment and the scope of your agent. Instead of forcing an agent to operate within a single folder, a project can work with one folder or multiple folders (for example, a frontend and a backend repository), providing your agents with all of the context required for your codebase. All projects have their own isolated agent settings, allowing you to customize each project’s security settings independently.

### Key differences: workspace vs. projects

| Feature | Original Model (Workspace) | New Model (Project) |
| :-- | :-- | :-- |
| **Organization Scope** | Tightly coupled to a single local repository. | Projects are a configuration of all of the context and folders that your agents should work with. |
| **Directory Boundaries** | Agent is strictly confined to one folder structure. | A single project can span **multiple folders** at once. |
| **Settings Isolation** | Settings inherited globally from the machine. | Projects have their own settings. Agents in a project use the project’s settings. |
| **Permissions** | Broad, global permissions. | Global permissions are inherited. Projects can have their own permissions in addition to global permissions. |
| **Customizations** | Skills/MCPs managed globally or per-workspace. | Reusable skills, MCPs, and hooks are managed globally. |

### Core project concepts

#### Folders

A project is composed of **folders**, which define the directories and repositories the agent is allowed to access:

*   **Local folders**: A folder that doesn’t have Git configured.
*   **Local Git checkout**: A folder that is a Git repository checkout.

#### Worktree selection (local vs. new worktree)

When starting a new conversation in a project, you choose how the agent interacts with your folders using the worktree selector:

*   **Local mode**: The agent works directly in your active local folders or Git checkouts. Best for quick, interactive edits in your current working folder.
*   **New worktree mode**: Creates a new Git worktree for the conversation. Best for complex tasks, keeping your active working folder untouched and preventing parallel subagents from conflicting.

#### Scoped settings and permissions

Settings and permissions are both scoped at the project level:

*   **Settings**: When a project is created, it starts with the **Inherit General** permission preset, following your global permission settings, with read and write access to all of your project’s folders. On macOS and Linux under the **Default** preset, terminal commands run without prompting inside the [terminal sandbox](/docs/sandbox) and require approval to run outside it; on Windows, the agent asks for permission to run terminal commands. These settings can be modified and apply to all agents within this project.
*   **Permissions**: Projects inherit global permissions, and you can augment them at the project level so agents only have the exact access required for that specific project’s tasks.

#### Workflows using projects

You can configure projects to support several common development workflows:

*   **Working in a single folder**: Create a project with a folder and then configure its settings.
*   **Working in multiple folders**: Add all related folders into a single project so the agent has full context across your codebases.
*   **Running parallel agents on the same folder**: Choose **Local Mode** when starting an agent so that all of your agents work in the same active folders.
*   **Isolating concurrent agents**: Choose **New Worktree Mode** when starting an agent so that separate, isolated Git worktrees are provisioned for each agent session, avoiding conflicts between agents.
*   **Mixed checkouts and local folders**: Working locally operates directly in the existing folders. Using **New Worktree Mode** spawns a new Git worktree for all active Git checkouts, allowing the agent to operate inside the new worktrees and the existing non-Git local folders simultaneously.

## Antigravity CLI

### Launching sessions with projects

#### Default project execution

When starting the CLI without any project flags, all conversations in the session run in the `default-cli-project`:

```bash
agy
```

#### Opening a session in a specific project

To open a session attached to a specific existing project, pass the `--project` flag with the target project ID:

```bash
agy --project=<project_id>
```

#### Creating a new project on startup

To create a new project and initialize your CLI session inside it, pass the `--new-project` flag:

```bash
agy --new-project
```

#### Resuming an existing conversation

If you resume a conversation (whether on startup using `--conversation=<conv_id>` or during a session using `/resume`), the CLI automatically uses the conversation’s associated project.

### Moving conversations between projects (`/fork`)

While interacting in an active session, you can copy and continue your current conversation in a different project using the `/fork` slash command:

```text
/fork <project_id>
```

When executed, the CLI forks your current conversation and associates the newly created conversation with `<project_id>`.