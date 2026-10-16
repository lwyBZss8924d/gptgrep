import { execFile, spawn } from 'node:child_process'
import { Transform } from 'node:stream'
import { StringDecoder } from 'node:string_decoder'
import { fileURLToPath } from 'node:url'

/** A recognized native failure whose structured evidence must reach the caller. */
export class NativeFailure extends Error {
  constructor(report, exitCode) {
    super(`Native gptgrep failed with exit ${exitCode}.`)
    this.name = 'NativeFailure'
    this.report = report
    this.exitCode = exitCode
  }
}

function recognizedFailure(report, command) {
  if (command === 'status' && report.schema_version === 'gptgrep.v1')
    return report.source_fresh === false && typeof report.generation === 'string'
      && Array.isArray(report.stale_files) && report.stale_files.length > 0
      && report.stale_files.every(path => typeof path === 'string' && path.length > 0)
  if (command === 'index' && report.schema_version === 'gptgrep.rich-ingest.v1')
    return typeof report.plan_sha256 === 'string' && Array.isArray(report.documents)
      && Array.isArray(report.blockers) && (report.blockers.length > 0
        || ['failed_unpublished', 'blocked_unpublished'].includes(report.status))
  if (command === 'enrich' && report.schema_version === 'gptgrep.enrich.v1')
    return ['incomplete', 'failed'].includes(report.status) && typeof report.ledger_path === 'string'
      && typeof report.plan_sha256 === 'string' && report.builder !== null && typeof report.builder === 'object'
      && report.jev !== null && typeof report.jev === 'object'
  if (report.schema_version === 'gptgrep.error.v1')
    return report.ok === false && typeof report.code === 'string' && report.code.length > 0
      && typeof report.error === 'string' && report.error.length > 0
  if (command === 'rerank' && report.schema_version === 'gptgrep.rich_rerank.response.v1')
    return typeof report.code === 'string' && report.code.length > 0
      && typeof report.message === 'string' && report.message.length > 0
      && (report.report === null || (report.report !== null && typeof report.report === 'object' && !Array.isArray(report.report)))
  if (command === 'batch' && ['gptgrep.batch.response.v1', 'gptgrep.batch.rich.response.v1'].includes(report.schema_version)
      && Array.isArray(report.items) && report.items.every((item) => item !== null && typeof item === 'object' && !Array.isArray(item))) {
    const failure = (error) => error !== null && typeof error === 'object'
      && typeof error.code === 'string' && error.code.length > 0
      && typeof error.message === 'string' && error.message.length > 0
    return failure(report.error) || report.items.some((item) => ['error', 'not_executed'].includes(item.status) && failure(item.error))
  }
  return command === 'search' && report.schema_version === 'gptgrep.v1'
    && typeof report.query === 'string' && typeof report.mode === 'string'
    && Array.isArray(report.hits) && report.hits.every((hit) => hit !== null && typeof hit === 'object' && !Array.isArray(hit))
    && Array.isArray(report.coverage?.stale_files) && report.coverage.stale_files.length > 0
    && report.coverage.stale_files.every((item) => typeof item === 'string' && item.length > 0)
}

function exitCodeFor(error) {
  if (Number.isInteger(error?.code) && error.code > 0 && error.code < 256) return error.code
  if (error?.signal === 'SIGINT') return 130
  if (error?.signal === 'SIGTERM') return 143
  return 2
}

function selectedSecrets(extraNames = []) {
  return ['OPENROUTER_API_KEY', 'TYPESAFE_API_KEY', 'OPENAI_API_KEY', ...extraNames]
    .map((name) => process.env[name]).filter((value) => typeof value === 'string' && value.length > 0)
}

function redactor(secrets = selectedSecrets()) {
  function redact(value) {
    if (typeof value === 'string')
      return secrets.reduce((text, secret) => text.replaceAll(secret, '[REDACTED]'), value)
    if (Array.isArray(value)) return value.map(redact)
    if (value !== null && typeof value === 'object')
      return Object.fromEntries(Object.entries(value).map(([key, item]) => [redact(key), redact(item)]))
    return value
  }
  return redact
}

/** Invoke the native binary without a shell and return its JSON object. */
export function runNative(args, binary = process.env.GPTGREP_BIN || 'gptgrep', timeout = 5 * 60 * 1000, input, maxOutputBytes = 8 * 1024 * 1024) {
  if (input !== undefined && (typeof input !== 'string' || Buffer.byteLength(input, 'utf8') > 1048576))
    return Promise.reject(new Error('Native stdin must be a UTF-8 string of at most 1048576 bytes.'))
  if (!Number.isSafeInteger(timeout) || timeout < 0 || timeout > 2147483647 || !Number.isSafeInteger(maxOutputBytes) || maxOutputBytes < 1)
    return Promise.reject(new Error('Native process timeout or output byte budget is invalid.'))
  const redact = redactor()
  return new Promise((resolve, reject) => {
    let stdinError
    let finishInput
    const stdinDone = new Promise((done) => { finishInput = done })
    const child = execFile(binary, args, {
      encoding: 'utf8',
      maxBuffer: maxOutputBytes,
      timeout,
      windowsHide: true,
    }, async (error, stdout, stderr) => {
      await stdinDone
      process.removeListener('SIGINT', interrupt)
      process.removeListener('SIGTERM', terminate)
      if (stderr) process.stderr.write(redact(stderr))
      if (error?.code === 'ENOENT') {
        reject(new Error('Native gptgrep was not found. Install its release binary or set GPTGREP_BIN to its executable path.'))
        return
      }
      let data
      try {
        data = JSON.parse(stdout)
      } catch {
        reject(new Error(error
          ? `Native gptgrep failed (${error.signal || error.code || 'unknown'}); see stderr.`
          : 'Native gptgrep did not return valid JSON.'))
        return
      }
      if (data === null || typeof data !== 'object' || Array.isArray(data)) {
        reject(new Error('Native gptgrep returned a non-object JSON response.'))
        return
      }
      if (recognizedFailure(data, args[0])) {
        reject(new NativeFailure(redact(data), exitCodeFor(error)))
        return
      }
      if (input !== undefined && stdinError) {
        reject(new Error('Native batch stdin delivery failed; no successful result is available.'))
        return
      }
      // A grep-style no-match status is only successful with a recognizable
      // empty result payload. Never convert arbitrary exit-1 errors to success.
      const matches = data.hits ?? data.results
      const noMatch = args[0] === 'search' && error?.code === 1
        && Array.isArray(matches) && matches.length === 0
        && data.ok !== false && !Object.hasOwn(data, 'error')
      if (error && !noMatch) {
        reject(new Error(`Native gptgrep failed (${error.signal || error.code || 'unknown'}); see stderr.`))
        return
      }
      resolve(data)
    })
    const interrupt = () => child.kill('SIGINT')
    const terminate = () => child.kill('SIGTERM')
    process.once('SIGINT', interrupt)
    process.once('SIGTERM', terminate)
    // The child may reject an invalid request before consuming all of stdin.
    // Preserve its terminal JSON/error instead of allowing EPIPE to crash us.
    child.stdin.on('error', (error) => { stdinError = error; finishInput() })
    child.stdin.end(input, (error) => { stdinError ??= error; finishInput() })
  })
}

/** Foreground native protocol/file operation. It never emits a completion envelope. */
export function runNativeForeground(args, binary = process.env.GPTGREP_BIN || 'gptgrep', {
  timeout = 0, maxOutputBytes = 8 * 1024 * 1024, tokenEnv,
} = {}) {
  if (!Number.isSafeInteger(timeout) || timeout < 0 || timeout > 2147483647 || !Number.isSafeInteger(maxOutputBytes) || maxOutputBytes < 1)
    return Promise.reject(new Error('Native process timeout or output byte budget is invalid.'))
  if (process.platform === 'win32')
    return Promise.reject(new Error('Native foreground supervision requires POSIX process groups.'))
  const secrets = selectedSecrets(tokenEnv ? [tokenEnv] : [])
  const redact = redactor(secrets)
  const overlap = Math.max(0, ...secrets.map(secret => secret.length - 1))
  return new Promise((resolve, reject) => {
    const anchor = fileURLToPath(new URL('./foreground-child.js', import.meta.url))
    const child = spawn(process.execPath, [anchor, binary, ...args], {
      stdio: ['inherit', 'pipe', 'pipe', 'ipc'], detached: true, windowsHide: true,
    })
    let bytes = 0
    let stopCode
    let killTimer
    let timer
    let nativeResult
    let spawnError
    let anchorExited = false
    let groupStopped = false
    const stopGroup = () => {
      // This live anchor never voluntarily exits, so its PID cannot be recycled
      // before this one group signal. An unexpected exit revokes group ownership.
      if (anchorExited || groupStopped || !child.pid) return
      try {
        process.kill(-child.pid, 'SIGKILL')
        groupStopped = true
      } catch (error) {
        spawnError ??= new Error(`Native foreground group cleanup failed (${error.code || 'unknown'}).`)
      }
    }
    const stop = (signal, code) => {
      stopCode ??= code
      if (child.connected) child.send({ signal }, () => {})
      killTimer ??= setTimeout(stopGroup, 5000)
    }
    // Keep only a possible secret suffix between chunks; never buffer the entire
    // NDJSON stream, and redact secrets even when split across OS read boundaries.
    const output = () => {
      let pending = ''
      const decoder = new StringDecoder('utf8')
      return new Transform({
        transform(chunk, _encoding, done) {
          bytes += chunk.length
          if (bytes > maxOutputBytes) {
            if (stopCode === undefined) process.stderr.write('Native foreground output exceeded the wrapper byte budget.\n')
            stop('SIGTERM', 125)
            done()
            return
          }
          pending += decoder.write(chunk)
          let end = Math.max(0, pending.length - overlap)
          for (const secret of secrets) {
            let start = pending.indexOf(secret)
            while (start !== -1 && start < end) {
              if (start + secret.length > end) end = start
              start = pending.indexOf(secret, start + 1)
            }
          }
          this.push(redact(pending.slice(0, end)))
          pending = pending.slice(end)
          done()
        },
        flush(done) {
          if (bytes <= maxOutputBytes) this.push(redact(pending + decoder.end()))
          done()
        },
      })
    }
    child.stdout.pipe(output()).pipe(process.stdout, { end: false })
    child.stderr.pipe(output()).pipe(process.stderr, { end: false })
    child.on('message', message => {
      if (message?.type === 'native-exit') {
        nativeResult = message
        stopGroup()
      } else if (message?.type === 'spawn-error') {
        spawnError = new Error(message.code === 'ENOENT'
          ? 'Native gptgrep was not found. Install its release binary or set GPTGREP_BIN to its executable path.'
          : `Native gptgrep could not start (${message.code || 'unknown'}).`)
        stopGroup()
      }
    })
    child.once('exit', () => {
      anchorExited = true
      clearTimeout(killTimer)
      if (!groupStopped) {
        spawnError ??= new Error('Native foreground supervisor exited unexpectedly; group ownership is no longer known.')
        // Unknown descendants may still hold inherited pipes. Stop waiting on
        // them without scheduling a signal against the now-reaped anchor PID.
        child.stdout.destroy()
        child.stderr.destroy()
      }
    })
    const interrupt = () => stop('SIGINT', 130)
    const terminate = () => stop('SIGTERM', 143)
    process.on('SIGINT', interrupt)
    process.on('SIGTERM', terminate)
    const cleanup = () => {
      clearTimeout(timer)
      clearTimeout(killTimer)
      process.removeListener('SIGINT', interrupt)
      process.removeListener('SIGTERM', terminate)
    }
    if (timeout) timer = setTimeout(() => {
      process.stderr.write('Native foreground wrapper deadline elapsed; awaiting process termination.\n')
      stop('SIGTERM', 124)
    }, timeout)
    child.once('error', error => {
      cleanup()
      reject(new Error(error.code === 'ENOENT'
        ? 'Native gptgrep was not found. Install its release binary or set GPTGREP_BIN to its executable path.'
        : `Native gptgrep could not start (${error.code || 'unknown'}).`))
    })
    child.once('close', () => {
      cleanup()
      if (spawnError) { reject(spawnError); return }
      if (!groupStopped || (!nativeResult && stopCode === undefined)) {
        reject(new Error('Native foreground supervisor exited unexpectedly; group ownership is no longer known.'))
        return
      }
      resolve(stopCode ?? (Number.isInteger(nativeResult.code) ? nativeResult.code : exitCodeFor({ signal: nativeResult.signal })))
    })
  })
}
