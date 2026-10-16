import { z } from 'incur'

const text = z.string().min(1)
const integer = z.number().int().min(1).max(Number.MAX_SAFE_INTEGER)
const digest = z.string().regex(/^[a-f0-9]{64}$/).describe('Exact SHA256 of the selected native control or plan')
const object = z.record(z.string(), z.unknown())
export const maxTimeoutSeconds = 2147473
export const execution = z.object({
  timeoutSeconds: z.number().int().min(1).max(maxTimeoutSeconds).optional()
    .describe('Wrapper wall-clock deadline; native deadlines remain in force. Ordinary calls default to 300 seconds, host calls to their native timeout plus 10. Foreground/control-driven calls default to the native deadline.'),
  maxOutputBytes: integer.optional().describe('Maximum bytes per captured stdout/stderr, or combined foreground stream; default 8388608. Increase explicitly for large results.'),
}).strict().optional().describe('Wrapper process limits, separate from native model/resource budgets')

export const hostExtra = {
  codexStateDir: text.optional().describe('Existing native state directory with logs child; authentication remains in the account home'),
  codexPermissions: text.optional().describe('Existing named Codex permission profile; native runtime must acknowledge it'),
  maxInputBytes: integer.max(1048576).optional(),
  protocolTrace: text.optional().describe('Private bounded native protocol-method trace path'),
}
export const askExtra = {
  nativePtc: z.boolean().default(false).describe('Use native reader tools with an existing codexPermissions profile; native budgets apply'),
  experimentalQueryPlan: z.boolean().default(false),
  experimentalSourceContinuation: z.boolean().default(false),
  experimentalEvidenceRoles: z.boolean().default(false),
  navigationOverlaySha256: digest.optional(),
  navigationMaxJevCalls: integer.optional(),
  navigationMaxRequestBytes: integer.optional(),
  plannerModel: text.optional(),
}
export const indexExtra = {
  maxFiles: integer.optional(),
  rich: z.boolean().default(false),
  ocr: z.enum(['off', 'auto']).optional(),
  planOnly: z.boolean().default(false),
  resume: z.boolean().default(false),
  ocrRunId: text.optional(),
  ocrRuntimeProfile: text.optional(),
  ocrCodexBin: text.optional(),
  ocrCodexHome: text.optional(),
  ocrCodexStateDir: text.optional(),
  ocrMaxPages: integer.optional(),
  ocrTimeoutSeconds: integer.optional(),
  pageWindow: integer.optional(),
  expectedPlanSha256: digest.optional(),
  parseAgentConfig: text.optional(),
  expectedParseAgentConfigSha256: digest.optional(),
  documentAgentControls: text.optional(),
  expectedDocumentAgentControlsSha256: digest.optional(),
}

export const flags = (r, names) => names.flatMap((name) => {
  const value = r[name]
  const flag = name.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)
  return value === undefined || value === false ? [] : [value === true ? `--${flag}` : `--${flag}=${value}`]
})

export function extraCommands(root, host, decisionsBudget) {
  const control = { config: text, expectedControlSha256: digest }
  const documentControl = { ...control, documentAgentControl: text, expectedDocumentAgentControlSha256: digest, source: text }
  // Judge's real native front door accepts a file, not stdin. Preserve that contract.
  return {
    files: { schema: z.object({ root }).strict(), description: 'List indexed documents and identifiers.', args: r => ['files', '--json', '--', r.root] },
    parse: { schema: z.object({ file: text }).strict(), description: 'Parse one source document locally without publishing an index.', args: r => ['parse', '--json', '--', r.file] },
    'native-schema': { schema: z.object({}).strict(), description: 'Read the selected native binary contract, including rich batch and toolbox operations.', args: () => ['schema', '--json'] },
    judge: {
      schema: z.object({ input: text.describe('Native state/questions or multimodal request JSON file; native validation enforces 64 KiB'), model: text.optional(), decisionsBudget }).strict(),
      description: 'Submit the explicitly selected native Jev request file; provider effects and accounting remain native.',
      args: r => ['judge', ...flags(r, ['input', 'model', 'decisionsBudget']), '--json'],
    },
    'host-complete': {
      schema: z.object({ ...host, completion: z.object({ instructions: text, state: z.unknown(), schema: object }).strict() }).strict(),
      description: 'Run a schema-validated native local Codex completion; no citation assertion.',
      args: r => ['host-complete', '--input=-', ...flags(r, Object.keys(host)), '--json'],
      input: r => JSON.stringify(r.completion),
      timeout: r => (r.timeout + 10) * 1000,
    },
    enrich: {
      schema: z.object({
        root, ledgerPath: text, resume: z.boolean().default(false), rebuildFailedWindow: z.number().int().min(0).optional(),
        supportRetries: z.number().int().min(0).max(2).optional(), expectedPlanSha256: digest.optional(),
        planOnly: z.boolean().default(false), planOutput: text.optional(), windowBytes: integer.optional(),
        maxHintsPerWindow: integer.optional(), maxBuilderCalls: integer, maxJevCalls: integer,
        maxLedgerBytes: integer.optional(), maxWindowsPerRun: integer.optional(),
        deadlineSeconds: integer.max(86400).default(900), jevModel: text.optional(), decisionsBudget,
        codexBin: host.codexBin, codexHome: host.codexHome, codexStateDir: host.codexStateDir,
        builderModel: text.default('gpt-6-luna'), reasoningEffort: host.reasoningEffort, serviceTier: host.serviceTier,
        timeout: z.number().int().min(1).max(900).default(180), maxInputBytes: host.maxInputBytes,
      }).strict(),
      description: 'Plan or run source-bound navigation enrichment with an explicit ledger and caller budgets.',
      args: r => ['enrich', ...flags(r, ['ledgerPath', 'resume', 'rebuildFailedWindow', 'supportRetries', 'expectedPlanSha256', 'planOnly', 'planOutput', 'windowBytes', 'maxHintsPerWindow', 'maxBuilderCalls', 'maxJevCalls', 'maxLedgerBytes', 'maxWindowsPerRun', 'deadlineSeconds', 'jevModel', 'decisionsBudget', 'codexBin', 'codexHome', 'codexStateDir', 'builderModel', 'reasoningEffort', 'serviceTier', 'timeout', 'maxInputBytes']), '--json', '--', r.root],
      timeout: r => (r.deadlineSeconds + 10) * 1000,
    },
    'parse-agent serve': {
      schema: z.object(control).strict(),
      description: 'Foreground native NDJSON toolbox: inherit stdin, stream replies, and wait for native exit. The control owns admission and deadlines.',
      args: r => ['parse-agent', 'serve', ...flags(r, Object.keys(control))],
      foreground: true,
    },
    'parse-agent run': {
      schema: z.object({ ...control, source: text, mode: z.enum(['off', 'auto']).default('off'), output: text }).strict(),
      description: 'Complete deterministic parsing into the selected product file. Native success has no stdout; exit and file content stay distinct.',
      args: r => ['parse-agent', 'run', ...flags(r, [...Object.keys(control), 'source', 'mode', 'output']), '--json'],
      foreground: true,
    },
    'parse-agent agent': {
      schema: z.object({ ...documentControl, output: text }).strict(),
      description: 'Launch one native document-agent job with pinned controls; this does not publish an index.',
      args: r => ['parse-agent', 'agent', ...flags(r, [...Object.keys(documentControl), 'output']), '--json'],
      timeout: () => 0,
    },
    'parse-agent qualify': {
      schema: z.object({ ...documentControl, qualificationRequest: text, expectedQualificationRequestSha256: digest, evidenceOutput: text }).strict(),
      description: 'Write explicit private native document qualification evidence; acceptance remains separate.',
      args: r => ['parse-agent', 'qualify', ...flags(r, [...Object.keys(documentControl), 'qualificationRequest', 'expectedQualificationRequestSha256', 'evidenceOutput']), '--json'],
      timeout: () => 0,
    },
    'ocr serve': {
      schema: z.object({ config: text, tokenEnv: text.default('GPTGREP_OCR_SERVE_TOKEN').describe('Bearer-token environment variable NAME, never its value'), initializeLedger: z.boolean().default(false), expectedControlSha256: digest.optional() }).strict(),
      description: 'Supervise explicit native foreground OCR until exit; stream lifecycle records. Prepared is not listener-ready or job completion.',
      args: r => ['ocr', 'serve', ...flags(r, ['config', 'tokenEnv', 'initializeLedger', 'expectedControlSha256'])],
      foreground: true,
    },
  }
}

export function validateNativeOptions(name, r, raw) {
  if (['ask', 'summarize', 'host-complete'].includes(name) && !r.nativePtc && (r.timeout > 900 || r.maxToolCalls > 64))
    throw new Error('Timeout above 900 seconds or maxToolCalls above 64 requires ask nativePtc:true; native execution validates the selected mode.')
  if (name === 'ask') {
    if (r.nativePtc && !r.codexPermissions) throw new Error('nativePtc requires codexPermissions.')
    if ((r.experimentalSourceContinuation || r.navigationOverlaySha256) && (!r.experimentalQueryPlan || !r.document))
      throw new Error('Source continuation and navigation overlays require experimentalQueryPlan and document.')
    if ((r.plannerModel || r.experimentalEvidenceRoles) && !r.experimentalQueryPlan)
      throw new Error('plannerModel and experimentalEvidenceRoles require experimentalQueryPlan.')
    if ((r.navigationMaxJevCalls !== undefined || r.navigationMaxRequestBytes !== undefined) && !r.navigationOverlaySha256)
      throw new Error('Navigation bounds require navigationOverlaySha256.')
  }
  if (name === 'index') {
    const pins = ['parseAgentConfig', 'expectedParseAgentConfigSha256', 'documentAgentControls', 'expectedDocumentAgentControlsSha256']
    if (pins.some(key => r[key] !== undefined) && !pins.every(key => r[key] !== undefined))
      throw new Error('Document-agent indexing requires all four pinned parser/config registry fields.')
    if (r.parseAgentConfig && ['optimizeMerge', ...Object.keys(indexExtra).filter(key => key !== 'maxFiles' && !pins.includes(key))].some(key => Object.hasOwn(raw, key) && raw[key] !== false))
      throw new Error('Document-agent indexing cannot combine legacy rich/OCR or optimizeMerge controls.')
  }
  if (name === 'enrich') {
    if (r.planOutput && !r.planOnly) throw new Error('planOutput requires planOnly.')
    if (r.rebuildFailedWindow !== undefined && (!r.resume || r.planOnly))
      throw new Error('rebuildFailedWindow requires resume and cannot combine planOnly.')
  }
}

export const workflows = {
  schema_version: 'gptgrep.wrapper.workflows.v1',
  supervisor: 'runtime/codex/AGENTS.md',
  manifest: 'runtime/codex/manifest.json',
  workflows: [
    { skill: 'skills/gptgrep/SKILL.md', commands: ['doctor', 'native-schema', 'index', 'status'], purpose: 'Choose the native CLI or public Rust APIs for the caller-owned job.' },
    { skill: 'skills/gptgrep-document/SKILL.md', commands: ['parse', 'parse-agent serve', 'parse-agent run', 'parse-agent agent', 'index'], purpose: 'Prepare complete documents; active native jobs use their existing toolbox.' },
    { skill: 'skills/gptgrep-ocr/SKILL.md', commands: ['ocr serve', 'parse-agent serve'], purpose: 'Explicit admitted OCR; ordinary retrieval does not start OCR.' },
    { skill: 'skills/gptgrep-retrieval/SKILL.md', commands: ['search', 'tree', 'read', 'batch', 'rerank', 'judge', 'ask', 'summarize'], purpose: 'Retrieve source evidence and preserve actual delivery/citation rules.' },
  ],
  rust_sdk: { documentation: 'docs/rust-sdk.md', architecture: 'docs/architecture.md', crates: ['gptgrep-core', 'gptgrep-host', 'gptgrep-parse', 'gptgrep-index', 'gptgrep-pageindex', 'gptgrep-jev'], interface: 'Public Rust crate APIs; caller selects source SDK and Rust toolchain. No JavaScript FFI.', installed: null },
  components: [
    { name: 'LiteParse', interface: 'gptgrep parse / parse-agent and gptgrep-parse Rust API', standalone_binary_bundled: false },
    { name: 'tgrep-core', interface: 'gptgrep search / gptgrep-index Rust API', standalone_binary_bundled: false },
    { name: 'PageIndex structure', interface: 'gptgrep tree / gptgrep-pageindex Rust API', standalone_binary_bundled: false },
  ],
  availability: 'Source-relative references describe this repository. Native doctor reports selected runtime capabilities; this manifest does not claim installation.',
}
