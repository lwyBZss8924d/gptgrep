// Passed as pinned Bun --eval source; no file/module lookup in the sandbox.
function actorIsolationProbe(input) {
  const fs = require("node:fs");
  const fail = (code) => { process.exitCode = code; };
  const denied = new Set(["EACCES", "EPERM", "ENOENT", "ENOTDIR"]);
  const errorCode = (error) => typeof error?.code === "string" && /^[A-Z0-9_]{1,48}$/.test(error.code) ? error.code : "OPERATIONAL_ERROR";
  if (!input || !Array.isArray(input.allow) || !Array.isArray(input.deny) || !Array.isArray(input.write) || input.write.length !== 4 || input.write.some(row => !row || typeof row.path !== "string" || !row.path.startsWith("/") || typeof row.allow !== "boolean") || input.allow.length < 3 || input.allow.length > 16 || input.deny.length !== 7 || [...input.allow, ...input.deny].some(p => typeof p !== "string" || !p.startsWith("/"))) { fail(60); return; }
  for (const kind of ["allow", "deny"]) {
    for (let index = 0; index < input[kind].length; index++) {
      let fd, openError = null;
      try { fd = fs.openSync(input[kind][index], fs.constants.O_RDONLY | fs.constants.O_NONBLOCK); }
      catch (error) { openError = errorCode(error); }
      if (fd === undefined) {
        process.stdout.write(JSON.stringify({ kind, index, readable: false, error: openError }) + "\n");
        if (kind === "allow" || !denied.has(openError)) { fail(42); return; }
        continue;
      }
      // A successful private open is failure, even if later I/O/close fails.
      if (kind === "deny") {
        process.stdout.write(JSON.stringify({ kind, index, readable: true, error: null }) + "\n");
        try { fs.closeSync(fd); } catch {}
        fail(41); return;
      }
      let readError = null;
      try { if (fs.readSync(fd, Buffer.alloc(1), 0, 1, null) !== 1) readError = "UNEXPECTED_EOF"; }
      catch (error) { readError = errorCode(error); }
      try { fs.closeSync(fd); } catch (error) { readError ??= errorCode(error); }
      process.stdout.write(JSON.stringify({ kind, index, readable: readError === null, error: readError }) + "\n");
      if (readError !== null) { fail(42); return; }
    }
  }
  for (let index = 0; index < input.write.length; index++) {
    const row = input.write[index]; let fd, failure = null;
    try { fd = fs.openSync(row.path, fs.constants.O_WRONLY | fs.constants.O_CREAT | fs.constants.O_EXCL | fs.constants.O_NOFOLLOW, 0o600); }
    catch (error) { failure = errorCode(error); }
    const writable = fd !== undefined;
    if (writable) { try { fs.writeSync(fd, "synthetic actor sandbox write\n"); fs.closeSync(fd); const read = fs.openSync(row.path, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW); try { if (fs.readSync(read, Buffer.alloc(1), 0, 1, null) !== 1) failure = "UNEXPECTED_EOF"; } finally { fs.closeSync(read); } } catch (error) { failure = errorCode(error); } }
    process.stdout.write(JSON.stringify({ kind: "write", index, writable, error: failure }) + "\n");
    if (writable !== row.allow || writable && failure !== null || !writable && !new Set(["EACCES", "EPERM", "EROFS", "ENOENT", "ENOTDIR"]).has(failure)) { fail(43); return; }
  }
  process.stdout.write(JSON.stringify({ schema: "gptgrep.c4-actor-isolation.probe.v1", positiveReads: input.allow.length, deniedReads: input.deny.length, writeProbes: input.write.length }) + "\n");
}
