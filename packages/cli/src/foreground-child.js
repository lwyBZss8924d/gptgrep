// Internal process-group anchor. Keep this process alive until the parent has
// terminated its owned group, even when the native child exits before its peers.
import { spawn } from 'node:child_process'

if (typeof process.send !== 'function' || process.platform === 'win32') process.exit(2)
let native
let exited = false
const report = message => process.send(message, () => {})
process.on('message', message => {
  if (!exited && ['SIGINT', 'SIGTERM'].includes(message?.signal)) native?.kill(message.signal)
})
// The anchor never exits voluntarily. If its parent disappears, its own PID is
// still the group ID, so this cannot address a recycled group after reaping.
process.on('disconnect', () => process.kill(-process.pid, 'SIGKILL'))
native = spawn(process.argv[2], process.argv.slice(3), { stdio: 'inherit', windowsHide: true })
native.once('error', error => {
  exited = true
  report({ type: 'spawn-error', code: error.code || 'unknown' })
})
native.once('exit', (code, signal) => {
  exited = true
  report({ type: 'native-exit', code, signal })
})
