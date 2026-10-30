# CLI best practices

Master the workflows, prompt architectures, and local configuration choices to maximize agent velocity while maintaining robust control.

## Establish verification loops

The single most effective way to ensure reliable, correct modifications from an autonomous agent is to provide the agent with a local verification mechanism (such as unit tests, build commands, or formatting scripts).

Before asking the agent to implement a code change, follow these steps:

1.  Ensure your workspace directory has a test suite ready.
2.  If tests don’t exist, direct the agent to write a standard test block _first_.
3.  Once the agent proposes code, instruct it to run the local test command to verify its work.
4.  Watch the agent execute the command and iterate on the test outputs automatically.

```text
> Implement feature X in main.py. Run npm test afterward to verify the build.
```

## Explore, plan, then execute

Autonomous local agents operate with the highest accuracy when complex changes are partitioned into distinct exploration, planning, and execution phases:

*   **Exploration**: Ask the agent to explain how the target codebase resolves a particular problem or where an interface is defined before writing any changes.
*   **Planning**: Request an implementation plan. The agent lists targeted files, required dependencies, and logic overrides in an implementation plan artifact.
*   **Execution**: Once you approve the structured plan, direct the agent to apply the edits.

```text
> Explore how our router resolves `/docs/:page`. Write down an implementation plan to add `/docs/best-practices`.
```

## Enrich your prompting context

Give local agents high-fidelity indicators to narrow down reasoning boundaries and minimize token overhead.

### Target file autocompletion

Type `@` within your prompt box to trigger the **Interactive Path Suggestion** overlay. Highlighting and selecting a path imports the absolute workspace file path directly into your prompt. This helps the agent target its code searches.

### Attaching visual evidence

If you’re debugging visual UI issues, rendering bugs, or frontend layout inconsistencies, capture a screenshot or video recording, copy it, and press `ctrl+v` inside the prompt box to attach it. The agent consults the media file to diagnose the issue.

## Configure your workspace environment

Optimize your local workstation rules and security boundaries to match your engineering flow.

### Write a codebase rule file

Create a `GEMINI.md` or `AGENTS.md` file at your workspace root to outline specific directory standards, styling paradigms, test command parameters, and deprecation warnings. The agent automatically parses these rules on startup and consults them before suggesting changes.

### Establish structured permissions

Tune your safety barriers in `~/.gemini/antigravity-cli/settings.json` based on your project risk level:

*   **`request-review`** (default): Prompts you before executing any write operations, Bash commands, or remote network calls.
*   **`proceed-in-sandbox`**: Restricts all terminal executions to a secure sandbox containment ring. Safe commands execute autonomously, while risky commands prompt for reviews.
*   **`strict`**: Always prompts for all non-read operations, providing complete line-by-line transparency.

```json
{
    "toolPermission": "proceed-in-sandbox",
    "enableTerminalSandbox": true
}
```

## Manage TUI sessions proactively

Use active session navigation tools to recover from engineering dead-ends or course-correct intermediate agent loops.

### Course-correct early (`esc`)

If you watch an agent execute an incorrect search pattern or write code that deviates from your intentions, press the global escape hatch key `esc` immediately to interrupt the turn and regain focus of a clean prompt.

### Rewind history with `/rewind`

If an agent has made several successive changes that introduce build errors, you don’t need to discard the session. Type `/rewind` (or `/undo`) to roll back your conversation thread to a previous stable checkout.

### Branch experiments with `/fork`

If you’re unsure of the best implementation path, follow these steps:

1.  Reach a stable baseline thread.
2.  Type `/fork` to spin up a duplicate parallel session.
3.  Test your speculative code modifications in the branched session.
4.  If the approach fails, run `/resume` to swap back to your stable main branch.

## Automate and script

Antigravity CLI is designed to operate seamlessly within standard shell pipeline tools.

### Run non-interactive commands (`-p`)

To automate quick queries or integrate agents into Git hooks, use the one-shot prompt flag `-p`:

```bash
agy -p "Review this git diff and draft a conventional commit message" --cwd $(pwd)
```

### Fan out using parallel subagents

For large-scale sweeps or multi-file refactoring, direct the primary agent to spawn concurrent background subagents. The agent manager handles background threads autonomously while you continue working on your primary screen.

## Related resources

Learn how to configure settings and customize visual layouts:

*   **[Settings, rendering, and keybindings](/docs/cli/settings)**: Customize keyboard hotkeys and buffers.
*   **[Permissions and sandbox](/docs/cli/sandbox)**: Enforce filesystem containment.
*   **[Plugins and skills](/docs/cli/plugins)**: Create your own custom slash commands.