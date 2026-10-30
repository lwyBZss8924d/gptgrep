# Agent skills

Available on: Antigravity App, Antigravity CLI, Antigravity IDE

Skills are an [open standard](https://agentskills.io/home) for extending agent capabilities. A skill is a folder containing a `SKILL.md` file with instructions that the agent can follow when working on specific tasks.

## What are skills?

Skills are reusable packages of knowledge that extend what the agent can do. Each skill contains:

*   **Instructions**: Explicit protocols for how to approach a specific task.
*   **Best practices**: Conventions, style guidelines, and checklists to follow.
*   **Scripts and resources**: Optional helper scripts and data schemas the agent can execute.

When you start a conversation, the agent sees a list of available skills with their names and descriptions. If a skill looks relevant to your task, the agent reads the full instructions and follows them.

Note

**Note**: Antigravity defaults to `.agents/skills`, but still maintains backward compatibility for `.agent/skills`.

## Anatomy of a skill

Skills are organized as directory bundles containing a required `SKILL.md` file:

```text
my-skill/
├── SKILL.md       # Required skill instructions and metadata
├── scripts/       # Optional executable scripts
├── examples/      # Optional reference implementations
└── resources/     # Optional templates, schemas, or data files
```

### Manifest format (`SKILL.md`)

Every `SKILL.md` file begins with YAML frontmatter defining its name and triggering criteria:

```yaml
---
name: code-review
description: Reviews code changes for bugs, style issues, and best practices. Use when reviewing pull requests or checking code quality.
---

# Code Review Skill

When reviewing code, follow these steps:

## Review checklist

1. **Correctness**: Verify that the code satisfies specifications.
2. **Edge cases**: Ensure error conditions and boundaries are handled.
3. **Style**: Follow project naming and architectural patterns.
4. **Performance**: Identify potential bottlenecks or inefficiencies.
```

### Frontmatter fields

The YAML frontmatter supports the following fields:

| Field | Required | Description |
| :-- | :-- | :-- |
| `name` | No | A unique identifier for the skill (lowercase, hyphens for spaces). Defaults to the folder name if not provided. |
| `description` | Yes | A clear description of what the skill does and when to use it. This is what the agent sees when deciding whether to apply the skill. |

Tip

Write your description in third person and include keywords that help the agent recognize when the skill is relevant. For example: “Generates unit tests for Python code using pytest conventions.”

## How the agent uses skills

Skills follow a **progressive disclosure** pattern:

1.  **Discovery**: When a conversation starts, the agent sees a list of available skills with their names and descriptions.
2.  **Activation**: If a skill looks relevant to your task, the agent reads the full `SKILL.md` content.
3.  **Execution**: The agent follows the skill’s instructions while working on your task.

You don’t need to explicitly tell the agent to use a skill—it decides based on context. However, you can mention a skill by name if you want to ensure it’s used.

## Best practices

### Keep skills focused

Each skill should do one thing well. Instead of a “do everything” skill, create separate skills for distinct tasks.

### Write clear descriptions

The description is how the agent decides whether to use your skill. Make it specific about what the skill does and when it’s useful.

### Use scripts as black boxes

If your skill includes scripts, encourage the agent to run them with `--help` first rather than reading the entire source code. This keeps the agent’s context focused on the task.

### Include decision trees

For complex skills, add a section that helps the agent choose the right approach based on the situation.

## Skills by surface

Explore how to create and manage skills on your preferred surface:

### Antigravity App

#### Antigravity App skill locations

The Antigravity App loads skills from two primary locations:

| Location | Scope |
| :-- | :-- |
| `<workspace-root>/.agents/skills/<skill-folder>/` | Workspace-specific |
| `~/.gemini/config/skills/<skill-folder>/` | Global (all workspaces) |

Each scope serves a distinct purpose:

*   **Workspace skills**: Scoped to a single project and committed to version control to share across your engineering team.
*   **Global skills**: Available across all projects on your workstation.

#### Invoking skills in the Antigravity App

You can invoke skills in the Antigravity App in two ways:

*   **Autonomous invocation**: The agent automatically reads and follows relevant skills based on your prompt.
*   **Manual slash command**: Type `/<skill-name>` in the prompt panel to explicitly invoke a skill.

### Antigravity CLI

#### CLI skill locations

The Antigravity CLI features full support for local and global agent skills:

| Location | Scope |
| :-- | :-- |
| `<workspace-root>/.agents/skills/<skill-folder>/` | Workspace-specific |
| `~/.gemini/antigravity-cli/skills/<skill-folder>/` | Global (all workspaces) |
| `~/.gemini/antigravity-cli/plugins/<name>/skills/` | Plugin-provided skills |

#### Slash command conversion

Whenever you define a skill, the CLI **automatically converts it into a slash command** inside the interactive TUI. For example, creating a skill named `deploy-staging` immediately makes `/deploy-staging` available in the prompt box.

#### Managing skills with plugins

Skills can also be packaged inside plugins and managed with `agy plugin`:

```bash
# List installed plugins (pass --json to include bundled skill names and paths)
agy plugin list
agy plugin list --json

# Install a plugin from the official marketplace (defaults to @antigravity-plugins-official)
agy plugin install <plugin-name>

# Install a skill bundle from a GitHub URL or local plugin path
agy plugin install https://github.com/<owner>/<repo>
agy plugin install ./my-skills-plugin
```

### Antigravity IDE

#### Antigravity IDE skill locations

In the standalone Antigravity IDE, skills are discovered from the following locations:

| Location | Scope |
| :-- | :-- |
| `<workspace-root>/.agents/skills/<skill-folder>/` | Workspace-specific |
| `~/.gemini/config/skills/<skill-folder>/` | Global (all workspaces; legacy `~/.gemini/antigravity/skills/` is also supported) |

You can inspect all active skills from the **Customizations** menu in the agent side panel.