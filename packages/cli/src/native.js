import { execFile } from 'node:child_process'

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
  if (report.schema_version === 'gptgrep.error.v1')
    return report.ok === false && typeof report.code === 'string' && report.code.length > 0
      && typeof report.error === 'string' && report.error.length > 0
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

function redactor() {
  const secrets = ['OPENROUTER_API_KEY', 'TYPESAFE_API_KEY', 'OPENAI_API_KEY']
    .map((name) => process.env[name]).filter((value) => typeof value === 'string' && value.length > 0)
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
export function runNative(args, binary = process.env.GPTGREP_BIN || 'gptgrep', timeout = 5 * 60 * 1000, input) {
  if (input !== undefined && (typeof input !== 'string' || Buffer.byteLength(input, 'utf8') > 1048576))
    return Promise.reject(new Error('Native stdin must be a UTF-8 string of at most 1048576 bytes.'))
  const redact = redactor()
  return new Promise((resolve, reject) => {
    let stdinError
    let finishInput
    const stdinDone = new Promise((done) => { finishInput = done })
    const child = execFile(binary, args, {
      encoding: 'utf8',
      maxBuffer: 8 * 1024 * 1024,
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
