# Google Cloud SDK Metric Attribution Standards

When executing automated or agentic calls against Google Cloud Platform APIs or through the `gcloud` CLI, request attribution must be passed to track developer platform metrics:

## 1. Environment Variable Attribution
```sh
export CLOUDSDK_METRICS_REQUEST_ATTRIBUTION="agent-skills/1.0 (skill:gptgrep-remote-devbox)"
```

## 2. HTTP Header Attribution
When invoking REST endpoints directly:
```http
User-Agent: gptgrep-remote-devbox/1.0 (gzip)
X-Goog-Api-Client: gl-python/3.13.0 gdcl/1.0 skill/gptgrep-remote-devbox
```
Attribution enables granular quota management and billing observability without impacting authentication scopes or permissions.
