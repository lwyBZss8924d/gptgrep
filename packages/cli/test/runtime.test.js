import assert from 'node:assert/strict'
import { spawn, execFileSync } from 'node:child_process'
import { chmod, mkdtemp, rm, writeFile } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { after, before, test } from 'node:test'
import { fileURLToPath } from 'node:url'

const entry = fileURLToPath(new URL('../src/cli.js', import.meta.url))
let temporary
let binary
before(async () => {
  temporary = await mkdtemp(path.join(os.tmpdir(), 'gptgrep-runtime-wrapper-'))
  binary = path.join(temporary, 'fixture.mjs')
  await writeFile(binary, String.raw`#!/usr/bin/env node
const args = process.argv.slice(2)
const mode = args.find(arg => arg.startsWith('--config='))?.slice(9)
if (args[0] === 'parse-agent' && args[1] === 'run') {
  process.exitCode = mode === 'fail' ? 2 : 0
} else if (args[1] === 'serve') {
  if (mode === 'echo') {
    process.stdin.pipe(process.stdout)
  } else if (mode === 'anchor-crash') {
    console.log(JSON.stringify({native_pid:process.pid}))
    setTimeout(()=>process.kill(process.ppid,'SIGKILL'),30)
    setTimeout(()=>process.exit(0),1500)
  } else if (mode === 'orphan') {
    const {spawn}=await import('node:child_process')
    const descendant=spawn(process.execPath,['-e',"process.on('SIGTERM',()=>{});process.on('SIGINT',()=>{});setInterval(()=>{},1000);process.send('ready')"],{stdio:['ignore','ignore','ignore','ipc']})
    descendant.once('message',()=>console.log(JSON.stringify({status:'prepared',descendant_pid:descendant.pid})))
    process.on('SIGTERM',()=>process.exit(0))
    process.on('SIGINT',()=>process.exit(0))
  } else if (mode === 'fail') {
    console.log(JSON.stringify({schema_version:'gptgrep.error.v1',ok:false,code:'fixture_failed',error:'retained failure'}))
    process.exitCode = 7
  } else if (mode === 'secret') {
    const value = process.env.FIXTURE_OCR_TOKEN
    process.stdout.write(value.slice(0,4))
    setTimeout(() => { process.stdout.write(value.slice(4)+'\n'); process.stderr.write(value+'\n') },25)
  } else if (mode === 'flood') {
    process.stdout.write('x'.repeat(4096))
  } else {
    console.log(JSON.stringify({status:'prepared',listener_ready:false}))
    const timer = setInterval(() => {},1000)
    process.on('SIGTERM',()=>{ clearInterval(timer); console.log(JSON.stringify({status:'stopped'})) })
    process.on('SIGINT',()=>{ clearInterval(timer); console.log(JSON.stringify({status:'stopped'})) })
  }
} else if (args[0] === 'host-complete') {
  let input=''
  process.stdin.setEncoding('utf8')
  process.stdin.on('data', part=>input+=part)
  process.stdin.on('end',()=>console.log(JSON.stringify({args,input,completion:JSON.parse(input)})))
} else if (args[0] === 'index' && args.includes('--ocr=auto')) {
  console.log(JSON.stringify({schema_version:'gptgrep.rich-ingest.v1',status:'blocked_unpublished',plan_sha256:'a'.repeat(64),documents:[],blockers:['fixture_ocr_required'],new_ocr_executions:0,physical_provider_requests:null}))
  process.exitCode=2
} else if (args[0] === 'status' && args.includes('stale')) {
  console.log(JSON.stringify({schema_version:'gptgrep.v1',generation:'selected-generation',source_fresh:false,stale_files:['changed.md']}))
  process.exitCode=2
} else if (args[0] === 'enrich' && args.includes('--ledger-path=failed')) {
  console.log(JSON.stringify({schema_version:'gptgrep.enrich.v1',status:'incomplete',ledger_path:'failed',plan_sha256:'a'.repeat(64),builder:{attempts:1},jev:{cost_usd:null}}))
  process.exitCode=2
} else console.log(JSON.stringify({args}))
`)
  await chmod(binary, 0o755)
})
after(async () => { await rm(temporary, { recursive: true, force: true }) })

function start(args, { executable = binary, input = '', env = {}, signalAfterPrepared } = {}) {
  const child = spawn(process.execPath, [entry, ...args], {
    env: { ...process.env, GPTGREP_BIN: executable, NO_UPDATE_NOTIFIER: '1', ...env },
    stdio: ['pipe', 'pipe', 'pipe'],
  })
  return new Promise((resolve, reject) => {
    let stdout = '', stderr = '', signalled = false
    const timer = setTimeout(() => { child.kill('SIGKILL'); reject(new Error('wrapper fixture did not exit')) }, 8000)
    child.stdout.setEncoding('utf8').on('data', chunk => {
      stdout += chunk
      if (signalAfterPrepared && !signalled && stdout.includes('prepared')) {
        signalled = true
        child.kill(signalAfterPrepared)
      }
    })
    child.stderr.setEncoding('utf8').on('data', chunk => { stderr += chunk })
    child.once('error', reject)
    child.once('close', (code, signal) => { clearTimeout(timer); resolve({ code, signal, stdout, stderr }) })
    child.stdin.on('error', () => {})
    child.stdin.end(input)
  })
}
const invoke = (command, request = {}, options) => start([...command.split(' '), '--request', JSON.stringify(request), '--json'], options)
const successful = async (...args) => {
  const result = await invoke(...args)
  assert.equal(result.code, 0, result.stdout + result.stderr)
  return JSON.parse(result.stdout)
}

test('all native groups have local help, request schemas and manifest discovery', async () => {
  for (const command of ['files', 'parse', 'native-schema', 'judge', 'enrich', 'host-complete', 'parse-agent serve', 'parse-agent run', 'parse-agent agent', 'parse-agent qualify', 'ocr serve']) {
    const schema = await start(['request-schema', command, '--json'], { executable: '/missing/native' })
    assert.equal(schema.code, 0, schema.stdout)
    assert.equal(JSON.parse(schema.stdout).additionalProperties, false)
    assert.ok(JSON.parse(schema.stdout).properties.execution)
    const help = await start([...command.split(' '), '--help'], { executable: '/missing/native' })
    assert.equal(help.code, 0)
    assert.match(help.stdout, /--request/)
    assert.doesNotMatch(help.stdout, /--mcp|--update|Sync skill/)
  }
  const manifest = await start(['--llms', '--format', 'json'], { executable: '/missing/native' })
  assert.equal(manifest.code, 0)
  for (const name of ['parse-agent', 'ocr', 'host-complete', 'workflows']) assert.ok(manifest.stdout.includes(name))
  const catalog = JSON.parse((await start(['workflows', '--json'], { executable: '/missing/native' })).stdout)
  assert.equal(catalog.workflows.length, 4)
  assert.equal(catalog.rust_sdk.installed, null)
  assert.ok(catalog.components.every(component => component.standalone_binary_bundled === false))
})

test('file operations and judge preserve actual native argv including flag-like paths', async () => {
  for (const [command, request, args] of [
    ['files', {root:'--docs'}, ['files', '--json', '--', '--docs']],
    ['parse', {file:'--document.pdf'}, ['parse', '--json', '--', '--document.pdf']],
    ['native-schema', {}, ['schema','--json']],
    ['judge', {input:'--request.json',model:'chosen',decisionsBudget:'/existing ledger.json'}, ['judge','--input=--request.json','--model=chosen','--decisions-budget=/existing ledger.json','--json']],
  ]) assert.deepEqual((await successful(command, request)).args, args)
  const invalid = await invoke('judge', {judge:{state:'unsupported stdin'}})
  assert.notEqual(invalid.code,0)
  assert.match(invalid.stdout,/INVALID_REQUEST/)
})

test('document-agent pins and rich index flags stay explicit and mutually exclusive', async () => {
  const sha='a'.repeat(64)
  const controls={parseAgentConfig:'/controls/parser.json',expectedParseAgentConfigSha256:sha,documentAgentControls:'/controls/documents.json',expectedDocumentAgentControlsSha256:sha}
  const indexed=await successful('index',{root:'/docs',maxFiles:300,...controls})
  for (const arg of ['--max-files=300','--parse-agent-config=/controls/parser.json','--document-agent-controls=/controls/documents.json']) assert.ok(indexed.args.includes(arg))
  assert.ok(!indexed.args.includes('--ocr-codex-bin=codex'))
  const rich=await successful('index',{rich:true,ocr:'off',planOnly:true,ocrMaxPages:200,ocrTimeoutSeconds:3600,pageWindow:7,expectedPlanSha256:sha,ocrCodexStateDir:'/state'})
  for(const arg of ['--rich','--ocr=off','--plan-only','--ocr-max-pages=200','--ocr-timeout-seconds=3600','--page-window=7','--ocr-codex-state-dir=/state']) assert.ok(rich.args.includes(arg))
  for(const request of [{parseAgentConfig:'/one'}, {...controls,rich:true}, {...controls,ocr:'off'}, {...controls,optimizeMerge:true}]) {
    const result=await invoke('index',request,{executable:'/missing/native'})
    assert.match(result.stdout,/INVALID_REQUEST/)
    assert.doesNotMatch(result.stdout,/NATIVE_ERROR/)
  }
  const r=await successful('parse-agent agent',{config:'/parser',expectedControlSha256:sha,documentAgentControl:'/document',expectedDocumentAgentControlSha256:sha,source:'--source',output:'/product'})
  assert.deepEqual(r.args,['parse-agent','agent','--config=/parser',`--expected-control-sha256=${sha}`,'--document-agent-control=/document',`--expected-document-agent-control-sha256=${sha}`,'--source=--source','--output=/product','--json'])
  const q=await successful('parse-agent qualify',{config:'/parser',expectedControlSha256:sha,documentAgentControl:'/document',expectedDocumentAgentControlSha256:sha,source:'source',qualificationRequest:'/purpose',expectedQualificationRequestSha256:sha,evidenceOutput:'/private'})
  assert.ok(q.args.includes('--qualification-request=/purpose') && q.args.includes('--evidence-output=/private'))
})

test('native PTC forwards caller budgets above retained host limits and checks dependencies', async () => {
  const r=await successful('ask',{question:'normal question',nativePtc:true,codexPermissions:'selected',codexStateDir:'/state',maxToolCalls:512,timeout:3600,maxInputBytes:500000,protocolTrace:'/trace',experimentalQueryPlan:true,plannerModel:'chosen',experimentalEvidenceRoles:true,document:'manual.pdf',experimentalSourceContinuation:true,navigationOverlaySha256:'b'.repeat(64),navigationMaxJevCalls:17,navigationMaxRequestBytes:64000})
  for(const arg of ['--native-ptc','--codex-permissions=selected','--codex-state-dir=/state','--max-input-bytes=500000','--protocol-trace=/trace','--experimental-query-plan','--planner-model=chosen','--experimental-evidence-roles','--experimental-source-continuation','--navigation-max-jev-calls=17']) assert.ok(r.args.includes(arg),arg)
  assert.equal(r.args[r.args.indexOf('--max-tool-calls')+1],'512')
  for(const request of [{nativePtc:true},{timeout:901},{maxToolCalls:65},{plannerModel:'chosen'},{experimentalSourceContinuation:true},{navigationMaxJevCalls:3}]) {
    const invalid=await invoke('ask',{question:'question',...request},{executable:'/missing/native'})
    assert.match(invalid.stdout,/INVALID_REQUEST/)
  }
})

test('host completion delivers one typed stdin object with only supported host flags', async () => {
  const completion={instructions:'literal --mcp $(touch sentinel)',state:{x:'界'},schema:{type:'object'}}
  const r=await successful('host-complete',{completion,codexStateDir:'/state',codexPermissions:'profile',maxInputBytes:600000})
  assert.equal(r.input,JSON.stringify(completion))
  assert.deepEqual(r.completion,completion)
  assert.ok(r.args.includes('--input=-') && r.args.includes('--max-input-bytes=600000'))
  assert.ok(!r.args.some(arg=>arg.startsWith('--jev-model=') || arg.startsWith('--document=')))
})

test('enrichment retains incomplete native attempts even with full-output filtering', async () => {
  const result=await start(['enrich','--request',JSON.stringify({ledgerPath:'failed',maxBuilderCalls:7,maxJevCalls:9,resume:true,rebuildFailedWindow:0,supportRetries:2}),'--json','--full-output','--filter-output','status'])
  assert.equal(result.code,2)
  const output=JSON.parse(result.stdout)
  assert.equal(output.ok,false)
  assert.equal(output.data.status,'incomplete')
  assert.equal(output.data.builder.attempts,1)
  assert.equal(output.data.jev.cost_usd,null)
  const plan=await successful('enrich',{ledgerPath:'/ledger',maxBuilderCalls:7,maxJevCalls:9,planOnly:true,planOutput:'/plan',deadlineSeconds:1800})
  assert.ok(plan.args.includes('--deadline-seconds=1800') && plan.args.includes('--plan-output=/plan'))
})

test('blocked index and stale status preserve native failure evidence', async () => {
  const index=await invoke('index',{ocr:'auto',planOnly:true})
  assert.equal(index.code,2)
  const report=JSON.parse(index.stdout)
  assert.equal(report.status,'blocked_unpublished')
  assert.deepEqual(report.blockers,['fixture_ocr_required'])
  assert.equal(report.physical_provider_requests,null)
  const status=await invoke('status',{root:'stale'})
  assert.equal(status.code,2)
  assert.deepEqual(JSON.parse(status.stdout).stale_files,['changed.md'])
})

test('foreground toolbox preserves NDJSON and does not append an incur success envelope', async () => {
  const input='{"id":"first"}\n{"id":"second","data":"界"}\n'
  const result=await start(['parse-agent','serve','--request',JSON.stringify({config:'echo',expectedControlSha256:'a'.repeat(64)}),'--json','--full-output','--filter-output','id'],{input})
  assert.equal(result.code,0)
  assert.equal(result.stdout,input)
  const silent=await invoke('parse-agent run',{config:'silent',expectedControlSha256:'a'.repeat(64),source:'source',output:'/native-product'})
  assert.equal(silent.code,0)
  assert.equal(silent.stdout,'')
  const failed=await invoke('parse-agent run',{config:'fail',expectedControlSha256:'a'.repeat(64),source:'source',output:'/native-product'})
  assert.equal(failed.code,2)
  assert.equal(failed.stdout,'')
})

test('foreground OCR failure, signals and timeout never become successful startup', async () => {
  const failed=await invoke('ocr serve',{config:'fail'})
  assert.equal(failed.code,7)
  assert.equal(JSON.parse(failed.stdout).ok,false)
  for(const [signal,code] of [['SIGINT',130],['SIGTERM',143]]) {
    const result=await invoke('ocr serve',{config:'wait'},{signalAfterPrepared:signal})
    assert.equal(result.code,code)
    assert.deepEqual(result.stdout.trim().split('\n').map(JSON.parse).map(row=>row.status),['prepared','stopped'])
  }
  const timed=await invoke('ocr serve',{config:'wait',execution:{timeoutSeconds:1}})
  assert.equal(timed.code,124)
  assert.match(timed.stderr,/deadline elapsed/)
  assert.ok(timed.stdout.includes('stopped'))
})

test('foreground diagnostic redaction spans chunks and output budgets stop the child', async () => {
  const secret='fixture-secret-split-across-chunks'
  const r=await invoke('ocr serve',{config:'secret',tokenEnv:'FIXTURE_OCR_TOKEN'},{env:{FIXTURE_OCR_TOKEN:secret}})
  assert.equal(r.code,0)
  assert.equal(r.stdout,'[REDACTED]\n')
  assert.equal(r.stderr,'[REDACTED]\n')
  const over=await invoke('ocr serve',{config:'flood',execution:{maxOutputBytes:100}})
  assert.equal(over.code,125)
  assert.equal(over.stdout,'')
  assert.match(over.stderr,/byte budget/)
  const capture=await invoke('doctor',{execution:{maxOutputBytes:1}})
  assert.notEqual(capture.code,0)
  assert.match(capture.stdout,/NATIVE_ERROR/)
  assert.deepEqual((await successful('doctor',{execution:{maxOutputBytes:1024}})).args,['doctor','--json'])
})

test('foreground group cleanup survives native exit before a signal-ignoring grandchild', async () => {
  for (const signal of ['SIGINT', 'SIGTERM', null]) {
    const result=await invoke('ocr serve',{config:'orphan',execution:{timeoutSeconds:1}},signal ? {signalAfterPrepared:signal} : {})
    assert.equal(result.code,signal === 'SIGINT' ? 130 : signal === 'SIGTERM' ? 143 : 124)
    const {descendant_pid:pid}=JSON.parse(result.stdout.trim())
    assert.ok(Number.isInteger(pid))
    let state=''
    try { state=execFileSync('ps',['-p',String(pid),'-o','stat='],{encoding:'utf8'}).trim() } catch (error) {
      assert.equal(error.status,1)
    }
    // An orphan may briefly await OS reaping as a zombie, but cannot execute.
    const stopped=state === '' || state.startsWith('Z')
    if (!stopped) { try { process.kill(pid,'SIGKILL') } catch {} }
    assert.ok(stopped,`Owned descendant remained live after foreground completion: ${state}`)
  }
})

test('unexpected guardian exit errors without waiting on unknown inherited pipes', async () => {
  const started=Date.now()
  const result=await invoke('ocr serve',{config:'anchor-crash'})
  assert.notEqual(result.code,0)
  assert.match(result.stdout,/supervisor exited unexpectedly/)
  assert.ok(Date.now()-started<1200,'Wrapper waited on a pipe after group ownership became unknown')
})
