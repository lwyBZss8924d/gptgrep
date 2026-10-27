# Model Context Protocol (MCP)

Available on: Antigravity App, Antigravity CLI, Antigravity IDE

Antigravity supports the [Model Context Protocol (MCP)](https://modelcontextprotocol.io), an open standard that lets AI agents and editors securely connect to local developer tools, databases, file parsers, and external remote APIs. This integration provides the AI model with real-time context and execution capabilities beyond your immediate workspace.

In this guide, you’ll learn how to connect and configure MCP servers across Antigravity products.

## What is MCP?

MCP acts as a universal bridge between Antigravity and your broader development environment. Instead of manually copying and pasting database schemas, logs, or API specifications into prompts or chat panels, MCP lets Antigravity fetch structured context directly or execute safe actions on your behalf when needed.

### Add context

With MCP, Antigravity can use live data from connected MCP servers to inform its reasoning and suggestions:

*   When writing a SQL query, Antigravity can inspect your live Neon, Supabase, or AlloyDB schema to suggest correct table and column names.
*   When debugging deployment failures, Antigravity can pull recent build logs directly from Netlify or Heroku.

### Add custom tools

With MCP, Antigravity can execute specific, safe actions defined by your connected servers:

*   Create a Linear issue for a `TODO` item.
*   Search Notion or GitHub for authentication patterns.

## Getting started by surface

### Antigravity App

In the Antigravity App, you can manage your MCP servers through the **Installed MCP Servers** section of your **Settings**.

To view and update your MCP servers:

1.  Click the **Settings** button on the bottom left of your screen.
2.  Select **Customizations** and review the **Installed MCP Servers** section.

To install an MCP server from the **Installed MCP Servers** section:

1.  Click **Add MCP**. This connects you to the MCP Store, a searchable list of available MCP servers.
2.  Search or scroll down to an MCP server you’d like to install.
3.  Click **Add**.

To manage your MCP servers from this screen:

*   **Uninstall**: Click the trash can icon next to the MCP server in the list.
*   **Disable or enable**: Click the toggle switch next to the MCP server in the list.
*   **Refresh**: Click the refresh button.

### Antigravity CLI

Antigravity CLI supports both local `stdio` processes and remote host MCP server configurations. The simplest path to installing an MCP server on Antigravity CLI is by using the **Interactive MCP Manager**. You can also manually edit your global server setup or workspace-level `mcp_config.json`.

#### Interactive MCP manager

Type `/mcp` inside the prompt panel and press Enter to open the interactive **MCP Manager Overlay**. This panel lets you perform the following actions:

*   View live status rings for active, disconnected, or loading servers.
*   Manually reload server configurations or inspect real-time connection logs.

#### Global and workspace server configs

Unlike legacy setups, Antigravity CLI separates MCP definitions into dedicated, sparse configurations:

*   **Global server setups**: Configured in `~/.gemini/config/mcp_config.json`.
*   **Workspace local setups**: Configured in your active project under `.agents/mcp_config.json`.

You can modify these files directly with your custom [MCP server configuration](/docs/mcp#mcp-configuration-structure).

Note

**Remote connection schema**: When declaring remote SSE, Streamable HTTP, or WebSocket-based MCP connections, you must define the `serverUrl` field. Legacy fields like `url` or `httpUrl` aren’t supported.

### Antigravity IDE

In Antigravity IDE, the easiest way to manage MCP servers is through the built-in MCP Store. In the MCP Store, you can browse, discover, and install supported MCP servers. You can also install custom servers by updating your `mcp_config.json`.

To use the MCP Store:

1.  Click **…** at the top of the editor’s agent side panel and select **MCP Servers**.
2.  Hover over any supported server and click **Install** (or click a server to view details and then click **Install**).
3.  Follow any on-screen prompts.

Once installed, resources and tools from the server are automatically available to the editor.

To connect to a custom MCP server not listed in the store:

1.  Click **…** at the top of the editor’s agent side panel and select **MCP Servers**.
2.  Click **Manage MCP Servers**.
3.  Click **View raw config**.
4.  Modify the `mcp_config.json` file with your custom [MCP server configuration](/docs/mcp#mcp-configuration-structure).

The configuration file is located globally at `~/.gemini/config/mcp_config.json` (or locally in your workspace under `.agents/mcp_config.json`).

Antigravity SDK

In Python applications built using the [Antigravity SDK](/docs/sdk/overview), you can connect MCP servers (`stdio`, `SSE`, or `HTTP`) programmatically under a unified execution pipeline alongside built-in tools and custom Python functions. The SDK automatically discovers servers configured in your workspace’s `.agents/mcp_config.json` file. For dedicated documentation and examples, refer to the [Antigravity SDK MCP guide](/docs/sdk/mcp).

## MCP configuration structure

Whether configuring custom servers for the Antigravity App, Antigravity IDE, or Antigravity CLI, the configuration file follows a standardized format. The file contains a single `mcpServers` object where you define each server you want to connect to:

```json
{
  "mcpServers": {
    "sqlite-explorer": {
      "command": "node",
      "args": ["/usr/local/bin/sqlite-mcp-server.js"],
      "env": {
        "SQLITE_DB_PATH": "/var/data/app.db"
      }
    },
    "my-remote-server": {
      "serverUrl": "https://api.example.com/mcp/",
      "headers": {
        "Authorization": "Bearer YOUR_API_TOKEN"
      }
    }
  }
}
```

### MCP configuration properties

Each server entry under `mcpServers` supports the following properties.

Require one of the following transport properties:

*   **`command`** (string): Path to the executable for `stdio` transport.
*   **`serverUrl`** (string): URL for remote `Streamable HTTP` or `SSE` servers.

Configure any of the following optional properties:

*   **`args`** (array of strings): Command-line arguments for `stdio` transport.
*   **`env`** (object): Environment variables for the `stdio` server process.
*   **`cwd`** (string): Working directory for `stdio` servers.
*   **`headers`** (object): Custom HTTP headers for remote servers.
*   **`authProviderType`** (string): Authentication provider. Supports `"google_credentials"` for Google Application Default Credentials (ADC).
*   **`oauth`** (object): OAuth client credentials (`clientId`, `clientSecret`).
*   **`disabled`** (boolean): Temporarily disable a server without removing its configuration.
*   **`disabledTools`** (array of strings): Tool names to withhold from the model.

## MCP authentication

Connected MCP servers can securely authenticate against external services using built-in Google credentials, automatic OAuth flows, or custom HTTP headers.

### Google credentials

Set `authProviderType` to `"google_credentials"` to use Google Application Default Credentials (ADC):

```json
{
  "mcpServers": {
    "my-gcp-service": {
      "serverUrl": "https://example.googleapis.com/mcp/",
      "authProviderType": "google_credentials"
    }
  }
}
```

This requires Application Default Credentials to be configured locally. To set them up, run:

```bash
gcloud auth application-default login
```

If you previously logged in, ensure your quota project is set by running:

```bash
gcloud auth application-default set-quota-project {QUOTA_PROJECT}
```

### OAuth

Antigravity automatically handles OAuth for servers that support dynamic client registration (DCR). For these servers, no additional configuration is needed:

```json
{
  "mcpServers": {
    "oauth-server": {
      "serverUrl": "https://api.example.com/mcp/"
    }
  }
}
```

If the server doesn’t support dynamic client registration, you can provide your client credentials manually:

```json
{
  "mcpServers": {
    "oauth-server": {
      "serverUrl": "https://api.example.com/mcp/",
      "oauth": {
        "clientId": "your-client-id",
        "clientSecret": "your-client-secret"
      }
    }
  }
}
```

If you provide client credentials manually, ensure the following URL is registered as a redirect URI in your OAuth provider:

```text
https://antigravity.google/oauth-callback
```

To connect to an OAuth-enabled server:

1.  Open [**Agent settings**](/docs/settings) with Cmd + , (macOS) or Ctrl + , (Windows and Linux).
    
2.  Navigate to the **Customizations** tab and click **Authenticate** next to the server.
    
    ![Click Authenticate](/assets/image/docs/tools/mcp-oauth-authenticate.png)
    
3.  Complete authentication in your browser and copy the authorization code.
    
    ![Copy authorization code](/assets/image/docs/tools/mcp-oauth-copy-code.png)
    
4.  Paste the code back into the settings panel and click **Submit**.
    
    ![Paste auth code](/assets/image/docs/tools/mcp-oauth-paste-code.png)
    

Once authenticated, the server reconnects automatically.

![Authenticated server](/assets/image/docs/tools/mcp-oauth-authenticated.png)

Access tokens are stored in `~/.gemini/antigravity/mcp_oauth_tokens.json`. Expired tokens are refreshed automatically, and invalid tokens are removed.

### Custom headers

For remote servers that require custom HTTP headers (such as API keys or bearer tokens), add them to the `headers` object. For example:

```json
{
  "mcpServers": {
    "my-remote-server": {
      "serverUrl": "https://api.example.com/mcp/",
      "headers": {
        "Authorization": "Bearer YOUR_API_TOKEN"
      }
    }
  }
}
```

## MCP permissions and access control

Access to Model Context Protocol tools and resources is governed by Antigravity’s [permissions system](/docs/permissions). By default, unconfigured MCP tools run in **Ask** mode, requiring your approval before execution. You can allow specific tools or entire servers in your policy configuration:

*   **`mcp(server/tool)`**: Matches a specific tool on a specific server.
*   **`mcp(server/*)`**: Matches all tools on a specified server.
*   **`mcp(*)`**: Global wildcard matching any MCP tool across all connected servers.

## Supported MCP servers

The MCP Store features direct integrations for a wide variety of developer platforms, databases, and productivity services:

**Databases and storage (15 servers):**

*   [AlloyDB for PostgreSQL](https://cloud.google.com/alloydb/docs/ai/use-alloydb-mcp)
*   [BigQuery](https://cloud.google.com/bigquery/docs/use-bigquery-mcp)
*   [Bigtable Admin remote MCP](https://docs.cloud.google.com/bigtable/docs/use-bigtable-mcp)
*   [ClickHouse](https://clickhouse.com/docs/use-cases/AI/MCP)
*   [Cloud SQL (MySQL, PostgreSQL, SQL Server, Managed)](https://docs.cloud.google.com/sql/docs/mysql/use-cloudsql-mcp)
*   [Dataplex](https://docs.cloud.google.com/dataplex/docs/use-remote-mcp)
*   [MCP Toolbox for Databases](https://mcp-toolbox.dev/documentation/introduction/)
*   [MongoDB](https://github.com/mongodb-js/mongodb-mcp-server)
*   [Neo4j](https://neo4j.com/docs/mcp/current/)
*   [Neon](https://github.com/neondatabase-labs/mcp-server-neon)
*   [Pinecone](https://github.com/pinecone-io/pinecone-mcp)
*   [Prisma](https://github.com/prisma/prisma?tab=readme-ov-file#mcp-server)
*   [Redis](https://github.com/redis/mcp-redis)
*   [Spanner](https://docs.cloud.google.com/spanner/docs/use-spanner-mcp)
*   [Supabase](https://github.com/supabase-community/supabase-mcp)

**Developer tools and CI/CD (17 servers):**

*   [Apigee MCP](https://docs.cloud.google.com/apigee/docs/reference/apis/apihub/mcp)
*   [Atlassian](https://support.atlassian.com/atlassian-rovo-mcp-server/docs/getting-started-with-the-atlassian-remote-mcp-server/)
*   [Cloud CLI Execution](https://docs.cloud.google.com/sdk/use-gcloud-mcp)
*   [Forge](https://app.softwareforge.ai/docs/introduction)
*   [GitHub](https://github.com/github/github-mcp-server)
*   [GitLab Orbit](https://docs.gitlab.com/orbit/remote/access/mcp/)
*   [GKE OneMCP](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/use-gke-mcp)
*   [Harness](https://github.com/harness/mcp-server)
*   [Heroku](https://github.com/heroku/heroku-mcp-server)
*   [Home Developer MCP](https://developers.home.google.com/mcp/developer)
*   [Linear](https://linear.app/changelog/2025-05-01-mcp)
*   [mabl](https://help.mabl.com/hc/en-us/articles/47299375773844-mabl-MCP-overview)
*   [Netlify](https://github.com/netlify/netlify-mcp)
*   [Opsera AI Agents](https://docs.agents.opsera.ai/)
*   [Postman](https://github.com/postmanlabs/postman-mcp-server)
*   [SonarQube](https://github.com/SonarSource/sonarqube-mcp-server)
*   [Vercel](https://vercel.com/docs/agent-resources/vercel-mcp)

**Frontend and design (8 servers):**

*   [Canva](https://www.canva.dev/docs/mcp/)
*   [Chrome DevTools](https://github.com/ChromeDevTools/chrome-devtools-mcp)
*   [Dart](https://dart.dev/tools/mcp-server)
*   [Figma Dev Mode MCP](https://help.figma.com/hc/en-us/articles/32132100833559-Guide-to-the-Dev-Mode-MCP-Server)
*   [Locofy](https://www.locofy.ai/docs/export-and-deployment/locofy-mcp/)
*   [Lovable MCP](https://docs.lovable.dev/integrations/lovable-mcp-server)
*   [Miro](https://developers.miro.com/docs/miro-mcp)
*   [Mobbin MCP](https://mobbin.com/mcp)

**Security (4 servers):**

*   [CrowdStrike](https://github.com/CrowdStrike/falcon-mcp)
*   [Endor Labs](https://docs.endorlabs.com/setup-deployment/mcp/antigravity)
*   [Splunk](https://splunkbase.splunk.com/app/7931)
*   [Wiz](https://www.wiz.io/blog/introducing-wiz-mcp)

**Analytics, AI, and cloud (32 servers):**

*   [Airweave](https://github.com/airweave-ai/airweave)
*   [Antimetal](https://docs.antimetal.com/connect)
*   [Arize](https://github.com/Arize-ai/arize-tracing-assistant)
*   [Cloud Audit Manager](https://docs.cloud.google.com/audit-manager/docs/reference/auditmanager/mcp)
*   [Courtroom5](https://courtroom5.com/)
*   [Daloopa](https://docs.daloopa.com/docs/mcp)
*   [Docusign](https://developers.docusign.com/platform/mcp-server/)
*   [Exa](https://docs.exa.ai/reference/exa-mcp)
*   [FactSet AI-Ready Data](https://developer.factset.com/mcp/factset-ai-ready-data-mcp)
*   [Finnhub](https://finnhub.io/mcp)
*   [Firebase](https://firebase.google.com/docs/ai-assistance/mcp-server)
*   [Fiscal.ai](https://docs.fiscal.ai/docs/guides/mcp-integration)
*   [Google Cloud Quotas](https://cloud.google.com/docs/quotas/overview)
*   [Grafana Cloud](https://github.com/grafana/ai-marketplace/tree/main/plugins/grafana-cloud-mcp)
*   [Guidepoint](https://github.com/guidepointglobal/mcp-skills-plugins)
*   [Harvey](https://developers.harvey.ai/guides/harvey_mcp)
*   [iManage](https://docs.imanage.com/ai/mcpserver-user-help/en-US/iManage_MCP_Server_Help.html)
*   [LegalZoom](https://www.legalzoom.com/ai)
*   [Looker](https://mcp-toolbox.dev/documentation/connect-to/ides/looker_mcp/)
*   [LSEG](https://developers.lseg.com/)
*   [NetDocuments](https://docs.cloud.google.com/gemini/enterprise/docs/connectors/netdocuments)
*   [Notion](https://github.com/makenotion/notion-mcp-server)
*   [Parallel Search](https://docs.parallel.ai/search/search-mcp)
*   [PayPal](https://developer.paypal.com/tools/mcp-server/)
*   [Perplexity Ask](https://github.com/ppl-ai/modelcontextprotocol)
*   [PostHog](https://posthog.com/mcp)
*   [Relativity](https://docs.cloud.google.com/gemini/enterprise/docs/connectors/relativity)
*   [S&P Global](https://github.com/kensho-technologies/kfinance)
*   [Salesforce](https://developer.salesforce.com/docs/platform/hosted-mcp-servers/guide/sobject-all.html)
*   [Sequential Thinking](https://github.com/modelcontextprotocol/servers/tree/main/src/sequentialthinking)
*   [Stripe](https://github.com/stripe/agent-toolkit/tree/main/modelcontextprotocol)
*   [Windsor AI](https://windsor.ai/documentation/windsor-mcp/)