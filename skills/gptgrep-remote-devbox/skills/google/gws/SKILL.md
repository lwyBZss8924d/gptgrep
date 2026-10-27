---
name: gws
description: >-
  Google Workspace CLI (gws) operator skill for autonomous interaction with Google Workspace APIs
  including Google Drive, Gmail, Google Calendar, Google Docs, Google Sheets, Google Slides, Google Tasks,
  and Google Chat via dynamic runtime discovery.
---

# Google Workspace CLI (`gws`) Skill

The `gws` skill enables agents and automated scripts to interact with Google Workspace APIs (Drive, Gmail, Calendar, Docs, Sheets, Slides, Tasks, Chat) using the official `gws` Rust CLI.

## Key Capabilities & Discovery Architecture

`gws` dynamically discovers API methods at runtime using Google Discovery Service JSON documents, eliminating hardcoded service SDK dependencies.

### Command Structure
```bash
gws <service> <resource> <method> [FLAGS] [OPTIONS]
```

### Supported Services
- `drive`: File management, upload/download, permissions, shared drives, export
- `gmail`: Search messages, send/reply/forward, triage, draft management, labels
- `calendar`: Agenda inspection, event insertion, availability search, recurring events
- `docs`: Document creation, text inspection, template substitution, batch updates
- `sheets`: Spreadsheet creation, tab management, range append/read, formula evaluation
- `slides`: Presentation creation, slide manipulation, layout management
- `tasks`: Task list management, task CRUD, status updates
- `chat`: Space messaging, thread replies, card creation
- `auth`: Credentials management (`login`, `logout`, `setup`, `status`, `export`)
- `schema`: Dynamic introspection of discovery schemas for any method

## Authentication & Credentials

`gws` supports multiple authentication mechanisms:
1. **OAuth2 Interactive Flow**: `gws auth login`
2. **Application Default Credentials (ADC)**: Reuses `gcloud auth application-default login`
3. **Encrypted Credential Store**: AES-256-GCM encrypted tokens in `GOOGLE_WORKSPACE_CLI_CONFIG_DIR` or `~/.config/gws`
4. **Environment Variables**: `GOOGLE_WORKSPACE_CLI_TOKEN`

## Common Automation Commands

### Inspect Status
```bash
gws auth status
```

### Google Drive
```bash
# List files with page size
gws drive files list --params '{"pageSize": 10}'

# Search files by query
gws drive files list --params '{"q": "name contains '\''report'\'' and trashed = false"}'
```

### Gmail
```bash
# Search unread messages
gws gmail users messages list --params '{"userId": "me", "q": "is:unread"}'

# Read specific message
gws gmail users messages get --params '{"userId": "me", "id": "MESSAGE_ID", "format": "full"}'
```

### Calendar
```bash
# List upcoming events
gws calendar events list --params '{"calendarId": "primary", "timeMin": "2026-10-22T00:00:00Z"}'
```

## Security & Safety Guardrails
- **Path Traversal Protection**: Paths passed to `--output-dir` or `--dir` are strictly validated against directory traversal (`../`).
- **Destructive Operation Confirmation**: Commands deleting resources (e.g. `files delete`) require explicit confirmation.
- **Attribution**: Automated requests should include metric attribution header where supported.
