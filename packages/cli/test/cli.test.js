import assert from 'node:assert/strict'
import { execFile } from 'node:child_process'
import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { createHash } from 'node:crypto'
import os from 'node:os'
import path from 'node:path'
import { promisify } from 'node:util'
import { after, before, test } from 'node:test'
import { fileURLToPath } from 'node:url'
import { runNative } from '../src/native.js'

const exec = promisify(execFile)
const entry = fileURLToPath(new URL('../src/cli.js', import.meta.url))
let temporary
let binary

before(async () => {
  temporary = await mkdtemp(path.join(os.tmpdir(), 'gptgrep-wrapper-'))
  binary = path.join(temporary, 'native-fixture')
  await writeFile(binary, `#!/usr/bin/env node
const args = process.argv.slice(2);
if (args.includes('unread-stdin-fixture')) {
  setInterval(() => {}, 1000);
} else if (args.includes('early-batch-rejection')) {
  process.stdout.write(JSON.stringify({schema_version: 'gptgrep.batch.rich.response.v1',
    error: {code: 'invalid_request', message: 'early fixture rejection'}, items: []}));
  process.exitCode = 2;
} else if (args[0] === 'batch' || args[0] === 'rerank') {
  let input = '';
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (part) => input += part);
  process.stdin.on('end', () => {
    const request = JSON.parse(input);
    if (request.fixtureFailure && args[0] === 'rerank') {
      process.stdout.write(JSON.stringify({schema_version:'gptgrep.rich_rerank.response.v1',code:'required_stage_failed',
        message:'fixture failure; partial observations retained',generation:null,report:{attempts:[{stage:'evidence_reranking'}]}}));
      process.exitCode=2;
    } else if (request.fixtureFailure) {
      process.stdout.write(JSON.stringify({schema_version: 'gptgrep.batch.rich.response.v1',
        execution: {scope: 'local_deterministic_no_model_context', model_submissions: 0},
        generation: null, error: {code: 'invalid_request', message: 'fixture request rejected'},
        items: [], read_metrics: null}));
      process.exitCode = 2;
    } else process.stdout.write(JSON.stringify({args, input, request}));
  });
} else if (args.includes('empty-result')) {
  process.stdout.write(JSON.stringify({hits: []})); process.exitCode = 1;
} else if (args.includes('native-error')) {
  process.stdout.write(JSON.stringify({error: 'failed', hits: []}));
  process.stderr.write('fixture diagnostic'); process.exitCode = 1;
} else if (args.includes('partial-accounting')) {
  const secret = process.env.OPENROUTER_API_KEY || '';
  process.stdout.write(JSON.stringify({schema_version: 'gptgrep.error.v1', ok: false,
    code: 'host_retrieval_failed', error: 'fixture rejected ' + secret,
    host_retrieval: {stage: 'evidence_reranking', ledger_path: '.gptgrep/receipts/jev-ledger.jsonl',
      jev: {requests: 1, calls_attempted: 2, usage: [{input_tokens: 321, output_tokens: 17, cost_usd: 0.0003}], accounting_complete: false},
      cause: {message: 'provider echoed ' + secret}}}));
  process.stderr.write('fixture stderr ' + secret); process.exitCode = 2;
} else if (args.includes('stale-result')) {
  process.stdout.write(JSON.stringify({schema_version: 'gptgrep.v1', query: 'stale-result', mode: 'regex',
    hits: [], coverage: {stale_files: ['old.md']}, metrics: {jev_calls_attempted: 0, jev_requests: 0}}));
  process.exitCode = 2;
} else if (args.includes('invalid-typed-failure')) {
  process.stdout.write(JSON.stringify({schema_version: 'gptgrep.error.v1', ok: true, code: 'wrong', error: {unrecognized: true}}));
  process.exitCode = 2;
} else process.stdout.write(JSON.stringify({args}));
`)
  await chmod(binary, 0o755)
})

after(async () => {
  await rm(temporary, { recursive: true, force: true })
})

function run(args, executable = binary, environment = {}) {
  return exec(process.execPath, [entry, ...args], {
    env: { ...process.env, GPTGREP_BIN: executable, NO_UPDATE_NOTIFIER: '1', ...environment },
  })
}

test('reserved flag-like query stays in native positional arguments', async () => {
  for (const query of ['--mcp', '--json', '--schema', 'search', '$(touch sentinel)']) {
    const { stdout } = await run(['search', '--request', JSON.stringify({ query, root: '/docs space', mode: 'lexical', limit: 7 }), '--json'])
    assert.deepEqual(JSON.parse(stdout).args,
      ['search', '--mode', 'lexical', '--limit', '7', '--context', '0', '--min-score', '0.5', '--json', '--', query, '/docs space'])
  }
})

test('all supported operations forward explicit native arguments', async () => {
  for (const [command, request, expected] of [
    ['index', { root: '/docs' }, ['index', '--json', '--', '/docs']],
    ['index', { root: '/docs', optimizeMerge: true }, ['index', '--optimize-merge', '--json', '--', '/docs']],
    ['status', { root: '/docs' }, ['status', '--json', '--', '/docs']],
    ['tree', { root: '/docs', file: 'a.pdf' }, ['tree', '--root=/docs', '--json', '--', 'a.pdf']],
    ['read', { root: '/docs', nodeId: '--schema', offset: 12, maxBytes: 1024 }, ['read', '--root=/docs', '--offset', '12', '--max-bytes', '1024', '--json', '--', '--schema']],
    ['doctor', {}, ['doctor', '--json']],
  ]) {
    const { stdout } = await run([command, '--request', JSON.stringify(request), '--json'])
    assert.deepEqual(JSON.parse(stdout).args, expected)
  }
})

test('rich batch reaches native stdin with exact pin, image operations and literal data', async () => {
  const batch = { schema_version: 'gptgrep.batch.rich.request.v1', generation: {generation: 'fixed-generation', manifest_sha256: 'a'.repeat(64)},
    operations: [{id: 'visual', op: 'inspect_visual', query: '图表 $(touch sentinel) --mcp'}, {id: 'bytes', op: 'read_visual', rendition_id: 'fixed-image'}] }
  for (const rich of [false, true]) {
    const {stdout} = await run(['batch', '--request', JSON.stringify({root: '/docs space', rich, batch}), '--json'])
    const result = JSON.parse(stdout)
    assert.deepEqual(result.args, ['batch', '--root=/docs space', '--input=-', ...(rich ? ['--rich'] : []), '--json'])
    assert.equal(result.input, JSON.stringify(batch))
    assert.deepEqual(result.request, batch)
  }
  const schema = JSON.parse((await run(['request-schema', 'batch', '--json'], '/missing/native')).stdout)
  assert.equal(schema.properties.rich.default, false)
  assert.equal(schema.properties.batch.type, 'object')
  const readSchema = JSON.parse((await run(['request-schema', 'read', '--json'], '/missing/native')).stdout)
  assert.equal(readSchema.properties.maxBytes.default, 8192)
  assert.equal(readSchema.properties.offset.default, 0)
})

test('native metadata projection forwards exact selectors and cursor without wrapper interpretation', async () => {
  const operation = {id: 'metadata', op: 'inspect_native_metadata', artifact_id: 'artifact-' + 'a'.repeat(64),
    fields: ['geometry', 'counts'], collection: 'path_segments', object_id: 'b'.repeat(64),
    item_fields: ['ordinal', 'raw_point', 'page_point'], page_size: 3, max_output_bytes: 8192, cursor: '0a12'}
  const batch = {schema_version: 'gptgrep.batch.rich.request.v1', generation: {generation: 'fixed', manifest_sha256: 'c'.repeat(64)}, operations: [operation]}
  const result = JSON.parse((await run(['batch', '--request', JSON.stringify({root: '/docs space', rich: true, batch}), '--json'])).stdout)
  assert.deepEqual(result.request, batch)
  assert.equal(result.input, JSON.stringify(batch))
  assert.deepEqual(result.args, ['batch', '--root=/docs space', '--input=-', '--rich', '--json'])
  const schema = JSON.parse((await run(['request-schema', 'batch', '--json'], '/missing/native')).stdout)
  assert.match(schema.properties.batch.description, /inspect_native_metadata/)
  assert.match(schema.properties.batch.description, /noncitable/)
  await assert.rejects(run(['batch', '--request', JSON.stringify({rich: true, batch: {...batch, fixtureFailure: true}}), '--json']), error => {
    assert.equal(error.code, 2)
    const report = JSON.parse(error.stdout)
    assert.equal(report.schema_version, 'gptgrep.batch.rich.response.v1')
    assert.equal(report.execution.model_submissions, 0)
    return true
  })
})

test('native batch failure retains complete structured evidence and nonzero exit', async () => {
  for (const options of [[], ['--full-output']]) {
    await assert.rejects(run(['batch', '--request', JSON.stringify({rich: true, batch: {fixtureFailure: true}}), '--json', ...options]), (error) => {
      assert.equal(error.code, 2)
      const outer = JSON.parse(error.stdout)
      const report = options.length ? outer.data : outer
      assert.equal(report.schema_version, 'gptgrep.batch.rich.response.v1')
      assert.equal(report.error.code, 'invalid_request')
      assert.deepEqual(report.items, [])
      assert.equal(report.execution.model_submissions, 0)
      if (options.length) assert.equal(outer.ok, false)
      return true
    })
  }
})

test('stdin UTF-8 byte cap rejects before starting an executable', async () => {
  await assert.rejects(runNative(['batch'], '/missing/native', 1000, '界'.repeat(349526)), /1048576 bytes/)
  await assert.rejects(runNative(['batch'], '/missing/native', 1000, {}), /UTF-8 string/)
})

test('early native rejection preserves JSON when stdin cannot finish', async () => {
  await assert.rejects(runNative(['batch', 'early-batch-rejection'], binary, 5000, 'x'.repeat(1048576)), (error) => {
    assert.equal(error.name, 'NativeFailure')
    assert.equal(error.exitCode, 2)
    assert.equal(error.report.error.message, 'early fixture rejection')
    return true
  })
})

test('native timeout settles even when the child does not consume stdin', async () => {
  await assert.rejects(runNative(['batch', 'unread-stdin-fixture'], binary, 150, 'x'.repeat(1048576)), /Native gptgrep failed/)
})

test('interrupt is forwarded while native stdin is blocked', async () => {
  const pending = runNative(['batch', 'unread-stdin-fixture'], binary, 5000, 'x'.repeat(1048576))
  const timer = setTimeout(() => process.emit('SIGINT'), 75)
  try { await assert.rejects(pending, /SIGINT/) } finally { clearTimeout(timer) }
})

test('default search selects hybrid while deterministic modes remain explicit', async () => {
  const result = JSON.parse((await run(['search', '--request', '{"query":"hello"}', '--json'])).stdout)
  assert.deepEqual(result.args, ['search', '--mode', 'hybrid', '--limit', '20', '--context', '0', '--min-score', '0.5', '--json', '--', 'hello', '.'])
  for (const mode of ['regex', 'lexical']) {
    const explicit = JSON.parse((await run(['search', '--request', JSON.stringify({ query: 'hello', mode }), '--json'])).stdout)
    assert.equal(explicit.args[explicit.args.indexOf('--mode') + 1], mode)
  }
  const semantic = JSON.parse((await run(['search', '--request', '{"query":"hello","mode":"semantic","minScore":0.75}', '--json'])).stdout)
  assert.equal(semantic.args[semantic.args.indexOf('--min-score') + 1], '0.75')
  for (const request of [{ query: 'hello', limit: 1001 }, { query: 'hello', minScore: 1.01 }]) {
    await assert.rejects(run(['search', '--request', JSON.stringify(request), '--json']),
      (error) => /INVALID_REQUEST/.test(error.stdout))
  }
})

test('schema discovery is local and exposes the JSON request contract', async () => {
  const { stdout } = await run(['request-schema', 'search', '--json'], '/missing/native')
  const schema = JSON.parse(stdout)
  assert.equal(schema.type, 'object')
  assert.equal(schema.properties.query.type, 'string')
  assert.deepEqual(schema.properties.mode.enum, ['regex', 'lexical', 'hybrid', 'semantic'])
  assert.equal(schema.properties.mode.default, 'hybrid')
  assert.equal(schema.properties.context.default, 0)
  assert.equal(schema.properties.model.type, 'string')
  assert.equal(schema.properties.document.type, 'string')
  const outer = JSON.parse((await run(['search', '--schema', '--json'], '/missing/native')).stdout)
  assert.equal(outer.options.properties.request.type, 'string')
  assert.equal(outer.env.properties.GPTGREP_BIN.type, 'string')
  const manifest = JSON.parse((await run(['--llms', '--format', 'json'], '/missing/native')).stdout)
  assert.ok(JSON.stringify(manifest).includes('search'))
})

test('explicit Codex host commands preserve model settings and flag-like data', async () => {
  const defaults = ['--codex-bin=codex', '--model=gpt-6-luna', '--reasoning-effort=max', '--service-tier=fast', '--timeout', '180', '--max-tool-calls', '12']
  const ask = JSON.parse((await run(['ask', '--request', '{"question":"--mcp","root":"/docs space"}', '--json'])).stdout)
  assert.deepEqual(ask.args, ['ask', ...defaults, '--json', '--', '--mcp', '/docs space'])
  const summary = JSON.parse((await run(['summarize', '--request', '{"nodeId":"document:node","root":"/docs","codexHome":"/account space"}', '--json'])).stdout)
  assert.deepEqual(summary.args, ['summarize', '--root=/docs', ...defaults, '--codex-home=/account space', '--json', '--', 'document:node'])
  const schema = JSON.parse((await run(['request-schema', 'ask', '--json'])).stdout)
  assert.equal(schema.properties.model.default, 'gpt-6-luna')
  assert.equal(schema.properties.reasoningEffort.default, 'max')
  assert.equal(schema.properties.serviceTier.default, 'fast')
  assert.equal(schema.properties.jevModel.type, 'string')
  assert.equal(schema.properties.document.type, 'string')
})

test('service tier is explicit and validated independently of model selection', async () => {
  const result = JSON.parse((await run(['ask', '--request', '{"question":"query","serviceTier":"flex"}', '--json'])).stdout)
  assert.ok(result.args.includes('--service-tier=flex'))
  await assert.rejects(run(['ask', '--request', '{"question":"query","serviceTier":"unknown"}', '--json']),
    (error) => /INVALID_REQUEST/.test(error.stdout))
})

test('Jev models and document scopes remain literal arguments and separate from Codex models', async () => {
  const document = 'notes/$(printf not-executed) --schema.md'
  const search = JSON.parse((await run(['search', '--request', JSON.stringify({ query: '--mcp', document, model: '~typesafe/jev-latest' }), '--json'])).stdout)
  assert.deepEqual(search.args, ['search', '--mode', 'hybrid', '--limit', '20', '--context', '0', '--min-score', '0.5', '--model=~typesafe/jev-latest', `--document=${document}`, '--json', '--', '--mcp', '.'])
  const host = ['--codex-bin=codex', '--model=gpt-6-astra', '--reasoning-effort=max', '--service-tier=fast', '--timeout', '180', '--max-tool-calls', '12', '--jev-model=typesafe/jev-1.13', `--document=${document}`]
  const common = { model: 'gpt-6-astra', jevModel: 'typesafe/jev-1.13', document }
  const ask = JSON.parse((await run(['ask', '--request', JSON.stringify({ question: '--json', ...common }), '--json'])).stdout)
  assert.deepEqual(ask.args, ['ask', ...host, '--json', '--', '--json', '.'])
  const summarize = JSON.parse((await run(['summarize', '--request', JSON.stringify({ nodeId: 'document:node', ...common }), '--json'])).stdout)
  assert.deepEqual(summarize.args, ['summarize', '--root=.', ...host, '--json', '--', 'document:node'])
  for (const [command, request] of [
    ['search', { query: 'hello', document: '' }],
    ['search', { query: 'hello', model: '' }],
    ['ask', { question: 'hello', jevModel: '' }],
  ]) {
    await assert.rejects(run([command, '--request', JSON.stringify(request), '--json']),
      (error) => /INVALID_REQUEST/.test(error.stdout))
  }
})

test('MCP and update routes are rejected before framework dispatch', async () => {
  for (const args of [['--mcp'], ['mcp', 'add'], ['--json', 'mcp', 'add'], ['skills', 'add'], ['--update']]) {
    await assert.rejects(run(args), (error) => {
      assert.equal(error.code, 2)
      assert.match(error.stderr, /MCP, skill sync and wrapper updates are disabled/)
      return true
    })
  }
  const help = (await run(['--help'])).stdout
  assert.doesNotMatch(help, /--mcp|--update|Register as MCP|Sync skill/)
})

test('invalid requests and missing native executable are actionable errors', async () => {
  await assert.rejects(run(['search', '--request', '{"query":"hi","limit":0}', '--json']),
    (error) => /INVALID_REQUEST/.test(error.stdout))
  await assert.rejects(run(['search', '--request', '{"query":"hi"}', '--json'], '/missing/native'),
    (error) => /NATIVE_ERROR/.test(error.stdout) && /not found/.test(error.stdout))
})

test('no-match exit is successful while native errors stay failures', async () => {
  const empty = await run(['search', '--request', '{"query":"empty-result"}', '--json'])
  assert.deepEqual(JSON.parse(empty.stdout), { hits: [] })
  await assert.rejects(run(['search', '--request', '{"query":"native-error"}', '--json']),
    (error) => /NATIVE_ERROR/.test(error.stdout) && /fixture diagnostic/.test(error.stderr))
})

test('typed exit-2 failure retains partial accounting and ledger with a nonzero status', async () => {
  const marker = 'fixture-only-sensitive-key-marker'
  for (const options of [[], ['--full-output'], ['--filter-output', 'schema_version']]) {
    const full = options.includes('--full-output')
    const args = ['ask', '--request', '{"question":"partial-accounting"}', '--json', ...options]
    await assert.rejects(run(args, binary, { OPENROUTER_API_KEY: marker }), (error) => {
      assert.equal(error.code, 2)
      assert.ok(!error.stdout.includes(marker) && !error.stderr.includes(marker))
      // Parsing the entire stdout also rejects any second emitted JSON object.
      const output = JSON.parse(error.stdout)
      if (full) {
        assert.equal(output.ok, false)
        assert.equal(output.meta.exitCode, 2)
      }
      const report = full ? output.data : output
      assert.equal(report.schema_version, 'gptgrep.error.v1')
      assert.equal(report.ok, false)
      assert.equal(report.host_retrieval.ledger_path, '.gptgrep/receipts/jev-ledger.jsonl')
      assert.deepEqual(report.host_retrieval.jev, {
        requests: 1, calls_attempted: 2,
        usage: [{ input_tokens: 321, output_tokens: 17, cost_usd: 0.0003 }], accounting_complete: false,
      })
      assert.match(report.error, /\[REDACTED\]/)
      return true
    })
  }
})

test('stale exit-2 search keeps its report while invalid typed failures remain generic', async () => {
  await assert.rejects(run(['search', '--request', '{"query":"stale-result","mode":"regex"}', '--json']), (error) => {
    assert.equal(error.code, 2)
    const report = JSON.parse(error.stdout)
    assert.equal(report.schema_version, 'gptgrep.v1')
    assert.deepEqual(report.coverage.stale_files, ['old.md'])
    assert.equal(report.metrics.jev_requests, 0)
    return true
  })
  await assert.rejects(run(['search', '--request', '{"query":"invalid-typed-failure"}', '--json']),
    (error) => error.code !== 0 && /NATIVE_ERROR/.test(error.stdout))
})


test('multimodal normal workflow forwards bounds and shared context without legacy window flags', async () => {
  const request = {query: '--chart $(touch sentinel)', root: '/docs space', multimodal: true,
    decisionsBudget: '/budget space/existing.json', document: 'report.pdf', multimodalTimeout: 120,
    windowBytes: 512, maxVisualCandidates: 3, maxConcurrentBatches: 2,
    maxOutputBytes: 900000, maxMetadataBytes: 32000, maxImageBytes: 500000, maxImagePixels: 4000000}
  const result = JSON.parse((await run(['search', '--request', JSON.stringify(request), '--json'])).stdout)
  assert.deepEqual(result.args, ['search', '--mode', 'hybrid', '--min-score', '0.5',
    '--decisions-budget=/budget space/existing.json', '--multimodal', '--multimodal-timeout=120',
    '--window-bytes=512', '--max-visual-candidates=3', '--max-concurrent-batches=2',
    '--max-output-bytes=900000', '--max-metadata-bytes=32000', '--max-image-bytes=500000', '--max-image-pixels=4000000',
    '--document=report.pdf', '--json', '--', request.query, request.root])
  const ask = JSON.parse((await run(['ask', '--request', JSON.stringify({question: 'Explain', multimodal: true,
    decisionsBudget: 'budget.json', maxCitations: 4, minScore: 0.7}), '--json'])).stdout)
  assert.ok(ask.args.includes('--model=gpt-6-luna'))
  assert.ok(ask.args.includes('--multimodal'))
  assert.ok(ask.args.includes('--decisions-budget=budget.json'))
  assert.ok(ask.args.includes('--max-citations=4'))
  assert.ok(ask.args.includes('--min-score=0.7'))
  const schema = JSON.parse((await run(['request-schema', 'search', '--json'], '/missing/native')).stdout)
  assert.equal(schema.properties.multimodal.default, false)
  assert.equal(schema.properties.maxVisualCandidates.maximum, 16)
  assert.equal(schema.properties.maxImagePixels.maximum, 8000000)
})

test('incompatible multimodal controls reject before spawning the native executable', async () => {
  for (const request of [
    {query: 'chart', maxVisualCandidates: 2},
    {query: 'chart', multimodal: true, mode: 'regex'},
    {query: 'chart', multimodal: true, mode: 'semantic'},
    {query: 'chart', multimodal: true, limit: 20},
    {query: 'chart', multimodal: true, context: 0},
    {query: 'chart', multimodal: true, fixedStrings: false},
    {query: 'chart', multimodal: true, maxImagePixels: 8000001},
  ]) await assert.rejects(run(['search', '--request', JSON.stringify(request), '--json'], '/missing/native'),
    (error) => /INVALID_REQUEST/.test(error.stdout) && !/NATIVE_ERROR/.test(error.stdout))
  await assert.rejects(run(['ask', '--request', '{"question":"Explain","maxCitations":4}', '--json'], '/missing/native'),
    (error) => /INVALID_REQUEST/.test(error.stdout))
})


test('paid rich batch preserves exact request and explicit original-budget arguments', async () => {
  const batch = {schema_version: 'gptgrep.batch.rich.request.v1', budget: {max_decisions_submissions: 2},
    operations: [{id: 'paid', op: 'search_multimodal', query: 'ordinary source question'}, {id: 'local', op: 'inspect_documents'}]}
  const request = {root: '/documents', rich: true, model: 'openai/gpt-6-luna-decisions',
    multimodalTimeout: 30, decisionsBudget: '/private-budget.json', batch}
  const result = JSON.parse((await run(['batch', '--request', JSON.stringify(request), '--json'])).stdout)
  assert.deepEqual(result.args, ['batch', '--root=/documents', '--input=-', '--rich',
    '--model=openai/gpt-6-luna-decisions', '--multimodal-timeout=30', '--decisions-budget=/private-budget.json', '--json'])
  assert.deepEqual(result.request, batch)
  for (const option of [{model:'openai/gpt-6-luna-decisions'}, {multimodalTimeout:30}])
    await assert.rejects(run(['batch','--request',JSON.stringify({batch,...option}),'--json'], '/missing/native'),
      (error) => /INVALID_REQUEST/.test(error.stdout) && !/NATIVE_ERROR/.test(error.stdout))
})


test('source-bound rerank forwards refs through bounded stdin with exact shared options',async()=>{
  const rerank={schema_version:'gptgrep.rich_rerank.request.v1',query:'ordinary source question',candidates:[{id:'label',evidence:{kind:'native_text',binding:{artifact_id:'source-ref'}}}]}
  const request={root:'/documents',model:'openai/gpt-6-luna-decisions',multimodalTimeout:30,decisionsBudget:'/existing-original-budget.json',rerank}
  const result=JSON.parse((await run(['rerank','--request',JSON.stringify(request),'--json'])).stdout)
  assert.deepEqual(result.args,['rerank','--root=/documents','--input=-','--model=openai/gpt-6-luna-decisions','--multimodal-timeout=30','--decisions-budget=/existing-original-budget.json','--json'])
  assert.equal(result.input,JSON.stringify(rerank));assert.deepEqual(result.request,rerank)
  const schema=JSON.parse((await run(['request-schema','rerank','--json'],'/missing/native')).stdout)
  assert.equal(schema.properties.rerank.type,'object');assert.equal(schema.additionalProperties,false)
})
test('source-bound rerank preserves structured partial failure and nonzero exit',async()=>{
  await assert.rejects(run(['rerank','--request',JSON.stringify({rerank:{fixtureFailure:true}}),'--json']),error=>{
    assert.equal(error.code,2);const r=JSON.parse(error.stdout);assert.equal(r.schema_version,'gptgrep.rich_rerank.response.v1')
    assert.equal(r.code,'required_stage_failed');assert.equal(r.report.attempts.length,1);assert.equal(r.ok,undefined);return true
  })
})
test('source-bound rerank wrapper refuses unknown fields and oversized stdin before native',async()=>{
  for(const request of [{rerank:{},text:'untrusted'},{rerank:{},multimodalTimeout:901}])
    await assert.rejects(run(['rerank','--request',JSON.stringify(request),'--json'],'/missing/native'),error=>{
      assert.match(error.stdout,/INVALID_REQUEST/);assert.doesNotMatch(error.stdout,/NATIVE_ERROR/);return true
    })
  await assert.rejects(runNative(['rerank'],'/missing/native',1000,'界'.repeat(349526)),/1048576 bytes/)
})

test('real native metadata wrapper composition preserves objects segments cursors and errors', {
  skip: !process.env.GPTGREP_NATIVE_INTEGRATION_BIN && !process.env.GPTGREP_NATIVE_INTEGRATION_FIXTURE && !process.env.GPTGREP_NATIVE_INTEGRATION_ROOT
    ? 'Explicit real native metadata binary, authored PDF and admitted published fixture were not supplied'
    : false,
}, async () => {
  const native = process.env.GPTGREP_NATIVE_INTEGRATION_BIN
  const fixture = process.env.GPTGREP_NATIVE_INTEGRATION_FIXTURE
  const root = process.env.GPTGREP_NATIVE_INTEGRATION_ROOT
  assert.ok(native && fixture && root, 'All exact real integration inputs are required')
  const binding = JSON.parse(await readFile(path.join(root, 'fixture-binding.json'), 'utf8'))
  assert.equal(binding.schema, 'gptgrep.synthetic.native_wrapper_fixture.v1')
  assert.equal(binding.synthetic_offline_ocr, true)
  const authored = await readFile(fixture)
  const retained = await readFile(path.join(root, binding.source_relative))
  assert.deepEqual(authored, retained)
  assert.equal(createHash('sha256').update(authored).digest('hex'), binding.source_sha256)
  const request = operation => ({schema_version: 'gptgrep.batch.rich.request.v1', generation: binding.generation, operations: [operation]})
  const direct = async batch => {
    try {
      const value = await runNative(['batch', '--rich', '--root=' + root, '--input=-', '--json'], native, 30000, JSON.stringify(batch))
      return value
    } catch (error) {
      if (error.report) return error.report
      if (error.stdout) return JSON.parse(error.stdout)
      throw error
    }
  }
  const wrapped = async batch => {
    try {
      return JSON.parse((await run(['batch', '--request', JSON.stringify({root, rich: true, batch}), '--json'], native)).stdout)
    } catch (error) {
      assert.equal(error.code, 2)
      return JSON.parse(error.stdout)
    }
  }
  const discover = await direct(request({id: 'pages', op: 'inspect_pages', document: binding.source_relative, fields: ['metadata_artifact']}))
  const artifact = discover.items[0].result.objects[0].metadata_artifact.id
  const base = {id: 'metadata', op: 'inspect_native_metadata', artifact_id: artifact,
    fields: ['geometry', 'content_bounds', 'counts'], collection: 'object_summaries',
    item_fields: ['id', 'kind', 'segment_count', 'page_bounds'], page_size: 3, max_output_bytes: 16384}
  // JavaScript JSON output canonicalizes signed zero; compare that one numeric
  // representation only, preserving every other value, key and array position.
  const normalizeSignedZero = value => {
    if (typeof value === 'number') return Object.is(value, -0) ? 0 : value
    if (Array.isArray(value)) return value.map(normalizeSignedZero)
    if (value && typeof value === 'object') {
      return Object.fromEntries(Object.entries(value).map(([key, entry]) => [key, normalizeSignedZero(entry)]))
    }
    return value
  }
  const nativeFirst = await direct(request(base))
  const wrapperFirst = await wrapped(request(base))
  assert.equal(wrapperFirst.items[0].status, 'ok')
  assert.deepEqual(wrapperFirst.items[0].result, normalizeSignedZero(nativeFirst.items[0].result))
  assert.deepEqual(wrapperFirst.items[0].coverage, nativeFirst.items[0].coverage)
  assert.equal(wrapperFirst.items[0].result.binding.source_sha256, binding.source_sha256)
  assert.equal(wrapperFirst.items[0].result.metadata_only, true)
  assert.equal(wrapperFirst.items[0].result.citation_authority, false)
  const next = {...base, cursor: nativeFirst.items[0].coverage.cursor}
  const nativeNext = await direct(request(next))
  const wrapperNext = await wrapped(request(next))
  assert.deepEqual(wrapperNext.items[0].result, normalizeSignedZero(nativeNext.items[0].result))
  const object = wrapperNext.items[0].result.rows.find(row => row.kind === 'path')
  assert.ok(object)
  const segments = {...base, collection: 'path_segments', object_id: object.id,
    fields: ['counts'], item_fields: ['ordinal', 'kind', 'raw_point', 'page_point', 'closes_subpath'], page_size: 1}
  const rows = []
  let operation = segments
  for (;;) {
    const expected = await direct(request(operation))
    const actual = await wrapped(request(operation))
    assert.equal(actual.items[0].status, 'ok')
    assert.deepEqual(actual.items[0].result, normalizeSignedZero(expected.items[0].result))
    assert.deepEqual(actual.items[0].coverage, expected.items[0].coverage)
    rows.push(...actual.items[0].result.rows)
    const cursor = actual.items[0].coverage.cursor
    if (!cursor) break
    operation = {...segments, cursor}
  }
  assert.equal(rows.length, object.segment_count)
  assert.deepEqual(rows.map(row => row.ordinal), rows.map((_, index) => index))
  for (const bad of [{...base, max_output_bytes: 256}, {...next, page_size: 4}, {...base, fields: ['unknown_field']}]) {
    const expected = await direct(request(bad))
    const actual = await wrapped(request(bad))
    assert.deepEqual(actual.error ?? actual.items[0].error, expected.error ?? expected.items[0].error)
    assert.ok(actual.error ?? actual.items[0].error)
  }
})
