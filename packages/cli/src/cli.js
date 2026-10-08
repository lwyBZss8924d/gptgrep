#!/usr/bin/env node
import { Cli, Formatter, z } from 'incur'

import { NativeFailure, runNative } from './native.js'

const argv = process.argv.slice(2)
let nativeFailureOutput
// incur has built-in MCP/update routes; guard them at our executable boundary.
// Reject those entry points before invoking it; never export a fetch handler.
if (argv.some((value) => ['--mcp', 'mcp', 'skills', '--update', '--incur-update-check'].includes(value))) {
  process.stderr.write('gptgrep-ai supports local CLI execution only; MCP, skill sync and wrapper updates are disabled.\n')
  process.exit(2)
}

const root = z.string().min(1).default('.').describe('Document corpus root')
const document = z.string().min(1).optional().describe('Exact indexed source-relative document path')
const decisionsBudget = z.string().min(1).optional().describe('Join an existing original-clock Decisions budget; native validation owns ledger admission')
const multimodal = {
  multimodal: z.boolean().default(false).describe('Source-bound v2 text/image routing and reranking; no legacy fallback'),
  multimodalTimeout: z.number().int().min(1).max(900).optional(),
  windowBytes: z.number().int().min(4).max(2048).optional(),
  maxVisualCandidates: z.number().int().min(1).max(16).optional(),
  maxConcurrentBatches: z.number().int().min(1).max(4).optional(),
  maxOutputBytes: z.number().int().min(1).max(1048576).optional(),
  maxMetadataBytes: z.number().int().min(1).max(524288).optional(),
  maxImageBytes: z.number().int().min(1).max(1048576).optional(),
  maxImagePixels: z.number().int().min(1).max(8000000).optional(),
}
const host = {
  codexBin: z.string().min(1).default('codex'),
  codexHome: z.string().min(1).optional().describe('Existing account home; native CODEX_HOME default applies when omitted'),
  model: z.string().min(1).max(128).default('gpt-6-luna').describe('Codex reasoning model; separate from the Jev Decisions model'),
  jevModel: z.string().min(1).max(128).optional().describe('Jev Decisions model; native default applies when omitted'),
  document,
  reasoningEffort: z.enum(['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra']).default('max'),
  serviceTier: z.enum(['fast', 'priority', 'flex', 'default']).default('fast'),
  timeout: z.number().int().min(1).max(900).default(180),
  maxToolCalls: z.number().int().min(1).max(64).default(12),
}
const requests = {
  search: z.object({
    query: z.string().describe('Retrieval query; JSON preserves flag-like text'),
    root,
    mode: z.enum(['regex', 'lexical', 'hybrid', 'semantic']).default('hybrid'),
    model: z.string().min(1).max(128).optional().describe('Jev Decisions model; native default applies when omitted'),
    document,
    limit: z.number().int().min(1).max(1000).default(20),
    context: z.number().int().min(0).max(100).default(0),
    minScore: z.number().min(0).max(1).default(0.5).describe('Jev relevance rubric floor; not calibrated confidence'),
    ignoreCase: z.boolean().default(false),
    fixedStrings: z.boolean().default(false),
    decisionsBudget,
    ...multimodal,
  }).strict(),
  index: z.object({ root, optimizeMerge: z.boolean().default(false) }).strict(),
  tree: z.object({
    file: z.string().min(1).describe('Source document path'),
    root,
  }).strict(),
  read: z.object({
    nodeId: z.string().min(1).describe('DOCUMENT_ID:NODE_ID from native search/tree output'),
    root,
    offset: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER).default(0),
    maxBytes: z.number().int().min(1).max(65536).default(8192),
  }).strict(),
  batch: z.object({
    root,
    rich: z.boolean().default(false).describe('Use the opt-in v2 rich batch contract'),
    model: z.string().min(1).max(128).optional().describe('Decisions model for explicit paid rich operations'),
    multimodalTimeout: z.number().int().min(1).max(900).optional(),
    decisionsBudget,
    batch: z.record(z.string(), z.unknown()).describe('Native batch request object; native validation owns its versioned operation schema'),
  }).strict(),
  status: z.object({ root }).strict(),
  doctor: z.object({}).strict(),
  ask: z.object({
    question: z.string().min(1).max(8192),
    root,
    ...host,
    decisionsBudget,
    ...multimodal,
    maxCitations: z.number().int().min(1).max(8).optional(),
    minScore: z.number().min(0).max(1).optional(),
  }).strict(),
  summarize: z.object({
    nodeId: z.string().min(1).describe('Verified DOCUMENT_ID:NODE_ID from native search/tree output'),
    root,
    ...host,
  }).strict(),
}

const descriptions = {
  search: 'Search with Jev hybrid retrieval by default; select regex or lexical for deterministic primitives.',
  index: 'Parse and index a document corpus with the native Rust pipeline.',
  tree: 'Read the indexed section tree for one source document.',
  read: 'Read a bounded source node window, preserving native evidence and continuation.',
  batch: 'Send a bounded native batch request through stdin; optionally use the rich v2 contract.',
  status: 'Inspect the current document generation.',
  doctor: 'Inspect native GPTgrep availability and configuration.',
  ask: 'Run an explicit bounded retrieval workflow through the local Codex host.',
  summarize: 'Summarize one verified node through the local Codex host.',
}

const hostArguments = (r) => [
  `--codex-bin=${r.codexBin}`, `--model=${r.model}`, `--reasoning-effort=${r.reasoningEffort}`,
  `--service-tier=${r.serviceTier}`,
  '--timeout', String(r.timeout), '--max-tool-calls', String(r.maxToolCalls),
  ...(r.codexHome === undefined ? [] : [`--codex-home=${r.codexHome}`]),
  ...(r.jevModel === undefined ? [] : [`--jev-model=${r.jevModel}`]),
  ...(r.document === undefined ? [] : [`--document=${r.document}`]),
]
const budgetArguments = (r) => r.decisionsBudget === undefined ? [] : [`--decisions-budget=${r.decisionsBudget}`]
const multimodalArguments = (r) => [
  ...(r.multimodal ? ['--multimodal'] : []),
  ...Object.entries({multimodalTimeout: 'multimodal-timeout', windowBytes: 'window-bytes',
    maxVisualCandidates: 'max-visual-candidates', maxConcurrentBatches: 'max-concurrent-batches',
    maxOutputBytes: 'max-output-bytes', maxMetadataBytes: 'max-metadata-bytes',
    maxImageBytes: 'max-image-bytes', maxImagePixels: 'max-image-pixels', maxCitations: 'max-citations'})
    .flatMap(([field, flag]) => r[field] === undefined ? [] : [`--${flag}=${r[field]}`]),
]
const argumentsFor = {
  search: (r) => ['search', '--mode', r.mode, ...(r.multimodal ? [] : ['--limit', String(r.limit), '--context', String(r.context)]), '--min-score', String(r.minScore), ...budgetArguments(r), ...multimodalArguments(r),
    ...(r.ignoreCase ? ['--ignore-case'] : []), ...(r.fixedStrings ? ['--fixed-strings'] : []),
    ...(r.model === undefined ? [] : [`--model=${r.model}`]),
    ...(r.document === undefined ? [] : [`--document=${r.document}`]),
    '--json', '--', r.query, r.root],
  index: (r) => ['index', ...(r.optimizeMerge ? ['--optimize-merge'] : []), '--json', '--', r.root],
  tree: (r) => ['tree', `--root=${r.root}`, '--json', '--', r.file],
  read: (r) => ['read', `--root=${r.root}`, '--offset', String(r.offset), '--max-bytes', String(r.maxBytes), '--json', '--', r.nodeId],
  batch: (r) => ['batch', `--root=${r.root}`, '--input=-', ...(r.rich ? ['--rich'] : []),
    ...(r.model === undefined ? [] : [`--model=${r.model}`]),
    ...(r.multimodalTimeout === undefined ? [] : [`--multimodal-timeout=${r.multimodalTimeout}`]),
    ...budgetArguments(r), '--json'],
  status: (r) => ['status', '--json', '--', r.root],
  doctor: () => ['doctor', '--json'],
  ask: (r) => ['ask', ...hostArguments(r), ...budgetArguments(r), ...multimodalArguments(r), ...(r.minScore === undefined ? [] : [`--min-score=${r.minScore}`]), '--json', '--', r.question, r.root],
  summarize: (r) => ['summarize', `--root=${r.root}`, ...hostArguments(r), '--json', '--', r.nodeId],
}

const cli = Cli.create('gptgrep-ai', {
  version: '0.1.0-alpha.1',
  description: 'Optional schema and formatting interface for native GPTgrep',
  format: 'json',
  config: false,
  sync: false,
  update: false,
})

for (const [name, schema] of Object.entries(requests)) {
  cli.command(name, {
    description: descriptions[name],
    mcp: false,
    env: z.object({
      GPTGREP_BIN: z.string().default('gptgrep').describe('Native executable path; never downloaded implicitly'),
    }),
    options: z.object({
      request: z.string().default('{}').describe(`JSON request object. Inspect with gptgrep-ai request-schema ${name} --json.`),
    }),
    output: z.record(z.string(), z.unknown()).describe('Native GPTgrep JSON response, preserved as structured data'),
    async run(c) {
      let request
      try {
        const raw = JSON.parse(c.options.request)
        request = schema.parse(raw)
        const visualKeys = Object.keys(multimodal).filter((key) => key !== 'multimodal')
        if (['search', 'ask'].includes(name) && !request.multimodal && [...visualKeys, 'maxCitations', ...(name === 'ask' ? ['minScore'] : [])].some((key) => Object.hasOwn(raw, key)))
          throw new Error('Multimodal bounds require multimodal:true.')
        if (name === 'batch' && !request.rich && ['model', 'multimodalTimeout'].some((key) => Object.hasOwn(raw, key)))
          throw new Error('Paid batch options require rich:true.')
        if (name === 'search' && request.multimodal && (request.mode !== 'hybrid'
            || ['limit', 'context', 'ignoreCase', 'fixedStrings'].some((key) => Object.hasOwn(raw, key))))
          throw new Error('Multimodal search requires hybrid mode and cannot combine legacy grep/window options.')
      } catch (error) {
        return c.error({ code: 'INVALID_REQUEST', message: error.message })
      }
      try {
        const timeout = name === 'ask' || name === 'summarize' ? (request.timeout + 10) * 1000 : undefined
        const input = name === 'batch' ? JSON.stringify(request.batch) : undefined
        if (input !== undefined && Buffer.byteLength(input, 'utf8') > 1048576)
          return c.error({ code: 'INVALID_REQUEST', message: 'Native batch input exceeds 1048576 UTF-8 bytes.' })
        return await runNative(argumentsFor[name](request), c.env.GPTGREP_BIN, timeout, input)
      } catch (error) {
        if (error instanceof NativeFailure) {
          process.exitCode = error.exitCode
          nativeFailureOutput = { report: error.report, exitCode: error.exitCode, format: c.format, command: name, written: false }
          return error.report
        }
        return c.error({ code: 'NATIVE_ERROR', message: error.message })
      }
    },
  })
}

cli.command('request-schema', {
  description: 'Return JSON Schema for the request object accepted by a command.',
  mcp: false,
  args: z.object({ command: z.enum(['search', 'index', 'tree', 'read', 'batch', 'status', 'doctor', 'ask', 'summarize']) }),
  output: z.record(z.string(), z.unknown()),
  run(c) {
    return z.toJSONSchema(requests[c.args.command])
  },
})

const help = argv.length === 0 || argv.includes('--help') || argv.includes('-h')
function writeNativeFailure() {
  if (!nativeFailureOutput || nativeFailureOutput.written) return
  nativeFailureOutput.written = true
  const { report, exitCode, format, command } = nativeFailureOutput
  const output = argv.includes('--full-output')
    ? { ok: false, data: report, meta: { command, exitCode } }
    : report
  process.stdout.write(`${Formatter.format(output, format)}\n`)
}
await cli.serve(argv, {
  // Published incur 0.5.1 still advertises disabled integration routes. Keep its
  // generated help accurate for this guarded wrapper, without changing JSON.
  stdout: (value) => {
    // incur's successful return-value path wraps data in ok:true. A recognized
    // native failure keeps its full report and cannot acquire that success flag.
    if (nativeFailureOutput) {
      writeNativeFailure()
      return
    }
    process.stdout.write(help
      ? value.split('\n').filter((line) => !/^  (?:mcp|skills|--mcp|--update)(?:\s|$)/.test(line)).join('\n')
      : value)
  },
})
// Preserve failure evidence even if an output filter suppressed normal rendering.
writeNativeFailure()
