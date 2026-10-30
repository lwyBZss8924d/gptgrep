# Google Workspace CLI (`gws`) Command Reference

This document summarizes the core commands, arguments, and parameters for the `gws` CLI.

## Global Options
- `--help`, `-h`: Show help documentation
- `--version`, `-V`: Print CLI version
- `--format [json|yaml|table|plain]`: Output serialization format (default: JSON)
- `--timezone <TZ>`: Explicit timezone resolution for calendar and timestamp calculations

## Authentication (`gws auth`)
- `gws auth login`: Launch OAuth browser flow
- `gws auth logout`: Remove stored credentials
- `gws auth status`: Check authorization state and token expiration
- `gws auth setup`: Configure client secrets and API scopes
- `gws auth export`: Export active access token for shell integrations

## Schema Introspection (`gws schema`)
- `gws schema <service>`: Inspect all resources and methods for a service
- `gws schema <service> <resource> <method>`: View JSON schema for request/response bodies and query parameters

## Service Commands Overview

### 1. Drive (`gws drive`)
- `gws drive files list [--params '<json>']`
- `gws drive files get --params '{"fileId": "ID"}'`
- `gws drive files create --json '<json>'`
- `gws drive files update --params '{"fileId": "ID"}' --json '<json>'`
- `gws drive files delete --params '{"fileId": "ID"}'`
- `gws drive files export --params '{"fileId": "ID", "mimeType": "application/pdf"}'`

### 2. Gmail (`gws gmail`)
- `gws gmail users messages list --params '{"userId": "me", "q": "..."}'`
- `gws gmail users messages get --params '{"userId": "me", "id": "..."}'`
- `gws gmail users messages send --params '{"userId": "me"}' --json '{"raw": "..."}'`
- `gws gmail users drafts list --params '{"userId": "me"}'`
- `gws gmail users labels list --params '{"userId": "me"}'`

### 3. Calendar (`gws calendar`)
- `gws calendar calendarList list`
- `gws calendar events list --params '{"calendarId": "primary", "timeMin": "..."}'`
- `gws calendar events insert --params '{"calendarId": "primary"}' --json '<event_json>'`
- `gws calendar events quickAdd --params '{"calendarId": "primary", "text": "Meeting tomorrow at 3pm"}'`

### 4. Docs (`gws docs`)
- `gws docs documents get --params '{"documentId": "ID"}'`
- `gws docs documents create --json '{"title": "..."}'`
- `gws docs documents batchUpdate --params '{"documentId": "ID"}' --json '{"requests": [...]}'`

### 5. Sheets (`gws sheets`)
- `gws sheets spreadsheets get --params '{"spreadsheetId": "ID"}'`
- `gws sheets spreadsheets create --json '{"properties": {"title": "..."}}'`
- `gws sheets spreadsheets values get --params '{"spreadsheetId": "ID", "range": "Sheet1!A1:D10"}'`
- `gws sheets spreadsheets values append --params '{"spreadsheetId": "ID", "range": "Sheet1!A:D", "valueInputOption": "USER_ENTERED"}' --json '{"values": [["val1", "val2"]]}'`

### 6. Tasks (`gws tasks`)
- `gws tasks tasklists list`
- `gws tasks tasks list --params '{"tasklist": "..."}'`
- `gws tasks tasks insert --params '{"tasklist": "..."}' --json '{"title": "..."}'`
