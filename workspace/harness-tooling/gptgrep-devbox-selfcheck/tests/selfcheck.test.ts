import { afterAll, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { appendFileSync, createReadStream, existsSync, mkdirSync, mkdtempSync, readFileSync, readSync, realpathSync, renameSync, rmSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { hostname } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { aggregate, parseConfig, PROBES, readSnapshot, verifyProbe } from "../src/selfcheck.ts";

// No implicit system tmp fallback: the owner storage runner must supply TMPDIR.
if (!process.env.TMPDIR) throw new Error("Tests require an admitted explicit TMPDIR");
const scratch = mkdtempSync(join(realpathSync(process.env.TMPDIR), "selfcheck-tests-"));
const repo = resolve(import.meta.dir, "../../../..");
const canonical = resolve(import.meta.dir, "../src/selfcheck.ts");
const marker = join(scratch, "unexpected-command");
const commands = join(scratch, "commands"); mkdirSync(commands);
for (const name of ["ssh", "gcloud", "docker", "codex", "claude", "systemctl"]) {
  writeFileSync(join(commands, name), `#!/bin/sh\nprintf invoked >> '${marker}'\nexit 91\n`, { mode: 0o755 });
}
symlinkSync(process.execPath, join(commands, "bun"));
const env = { ...process.env, PATH: `${commands}:${process.env.PATH}` };
const sha = (bytes: string | Buffer) => createHash("sha256").update(bytes).digest("hex");
async function fileHash(path: string) { const h = createHash("sha256"); for await (const part of createReadStream(path)) h.update(part); return h.digest("hex"); }
const tool = join(scratch, "pinned-tool");
writeFileSync(tool, `#!/bin/sh\nprintf unexpected >> '${marker}'\nexit 91\n`, { mode: 0o755 });
const storage = statSync(scratch);
function config() {
  return {
    schema: "gptgrep.devbox-selfcheck.config.v1",
    project_root: repo,
    // Different property order must not make an equal host fail.
    expected_host: { arch: process.arch, platform: process.platform, hostname: hostname() },
    storage: { path: scratch, device: storage.dev, inode: storage.ino, reserve_bytes: 1 },
    tools: [{ name: "fixture-tool", path: tool, sha256: sha(readFileSync(tool)) }],
    links: [],
  };
}
function writeConfig(value: unknown, name = "config.json") { const path = join(scratch, name); writeFileSync(path, JSON.stringify(value), { mode: 0o600 }); return path; }
function cli(args: string[], launcher?: string) {
  const argv = launcher ? [launcher, ...args] : [process.execPath, canonical, ...args];
  return spawnSync(argv[0], argv.slice(1), { env, encoding: "utf8", timeout: 15000, maxBuffer: 1024 * 1024 });
}
afterAll(() => { expect(existsSync(marker)).toBe(false); rmSync(scratch, { recursive: true, force: true }); });

describe("entrypoints and honest observation states", () => {
  test("all launchers target one canonical implementation and preserve basic help", () => {
    for (const path of ["bin/gptgrep-devbox-selfcheck", "workspace/harness-tooling/gptgrep-devbox-selfcheck/bin/gptgrep-devbox-selfcheck", "skills/gptgrep-remote-devbox/bin/gptgrep-devbox-selfcheck"]) {
      const absolute = join(repo, path), source = readFileSync(absolute, "utf8");
      expect(source.split("\n").length).toBeLessThan(25);
      const match = source.match(/exec bun "\$SELF_DIR\/(.+)" "\$@"/);
      expect(match).not.toBeNull();
      expect(realpathSync(resolve(dirname(absolute), match![1]))).toBe(realpathSync(canonical));
      const result = cli(["--help"], absolute);
      expect(result.status).toBe(0);
      expect(cli(["-h"], absolute).status).toBe(0);
      for (const option of ["--mode", "--json", "--strict", "--output-file", "--verbose", "--config"]) expect(result.stdout).toContain(option);
    }
  });
  test("copy-only installation carries its source closure through installed symlink chains", async () => {
    const home = join(scratch, "installed-home"), payload = join(home, "lib", "selfcheck");
    const install = spawnSync("sh", [resolve(import.meta.dir, "../install.sh"), payload], { env, encoding: "utf8", timeout: 5000 });
    expect(install.status).toBe(0);
    expect(await fileHash(join(payload, "src/selfcheck.ts"))).toBe(await fileHash(canonical));
    mkdirSync(join(home, "bin"), { recursive: true });
    symlinkSync("../lib/selfcheck/bin/gptgrep-devbox-selfcheck", join(home, "bin", "selfcheck"));
    const publicBin = join(scratch, "public-bin"); mkdirSync(publicBin);
    const entry = join(publicBin, "selfcheck"); symlinkSync(join(home, "bin", "selfcheck"), entry);
    expect(cli(["--help"], entry).status).toBe(0);
    const result = cli(["--json"], entry); expect(result.status).toBe(1);
    const receipt = JSON.parse(result.stdout);
    expect(receipt.binding.implementation.path).toBe(realpathSync(join(payload, "src/selfcheck.ts")));
    expect(receipt.binding.implementation.sha256).toBe(await fileHash(canonical));
    expect(receipt.overall_status).toBe("UNKNOWN");
  });
  test("missing configuration and remote mode never SSH, wake or fabricate readiness", () => {
    const result = cli(["--mode=remote", "--json", "--strict"]);
    expect(result.status).toBe(1);
    const receipt = JSON.parse(result.stdout);
    expect(receipt.schema).toBe("gptgrep.devbox-selfcheck.receipt.v2");
    expect(receipt.overall_status).toBe("UNKNOWN");
    expect(receipt.summary).toEqual({ total_gates: 5, passed_gates: 0, failed_gates: 0, unknown_gates: 5 });
    expect(receipt.execution_scope).toBe("current_host");
    expect(receipt.binding.host.hostname).toBe(hostname());
    expect(receipt.binding.mode).toBe("remote");
    expect(receipt.qualification.attempted).toBe(false);
    expect(receipt.workload_admission.status).toBe("not_granted");
    expect(existsSync(marker)).toBe(false);
  });
  test("valid file observations bind exact bytes while actual readiness remains unknown", async () => {
    const path = writeConfig(config());
    const result = cli(["--config", path, "--json"]);
    expect(result.status).toBe(1);
    const receipt = JSON.parse(result.stdout);
    expect(receipt.observations_status).toBe("PASSED");
    expect(receipt.overall_status).toBe("UNKNOWN");
    expect(receipt.binding.config.sha256).toBe(await fileHash(path));
    expect(receipt.binding.implementation.sha256).toBe(await fileHash(canonical));
    expect(receipt.binding.runtime.sha256).toBe(await fileHash(process.execPath));
    expect(receipt.binding.project.path).toBe(realpathSync(repo));
    expect(receipt.gates[1].observations[0].observed.version).toBeNull();
    expect(receipt.gates[0].observations[0].observed.available_bytes).toBeGreaterThan(0);
    expect(receipt.gates[2].observations).toEqual([]);
    expect(existsSync(marker)).toBe(false);
  });
  test("a wrong host or storage identity is an observed failure", () => {
    const value = config(); value.expected_host.hostname = "deliberately-wrong-target"; value.storage.inode += 1;
    const result = cli(["--config", writeConfig(value, "wrong.json"), "--json"]);
    expect(result.status).toBe(1);
    const receipt = JSON.parse(result.stdout);
    expect(receipt.overall_status).toBe("FAILED");
    expect(receipt.gates[0].status).toBe("FAILED");
    expect(receipt.gates[4].status).toBe("FAILED");
  });
  test("static status fields, placeholders and client auth files cannot satisfy config", () => {
    expect(() => parseConfig({ ...config(), overall_status: "PASSED" })).toThrow();
    expect(() => parseConfig({ ...config(), tools: [] })).toThrow();
    const auth = join(scratch, "auth.json"); writeFileSync(auth, JSON.stringify(config()));
    const alias = join(scratch, "auth-alias.json"); symlinkSync(auth, alias);
    const result = cli(["--config", alias, "--json"]);
    expect(result.status).toBe(1);
    expect(JSON.parse(result.stdout).observations_status).toBe("UNKNOWN");
  });
  test("output is private, exclusive and unknown stays nonzero", () => {
    const path = join(scratch, "receipt.json"), args = ["--config", writeConfig(config()), "--json", "--output-file", path];
    const first = cli(args); expect(first.status).toBe(1);
    expect(statSync(path).mode & 0o777).toBe(0o600);
    const original = readFileSync(path, "utf8"); expect(JSON.parse(original)).toEqual(JSON.parse(first.stdout));
    expect(cli(args).status).toBe(2); expect(readFileSync(path, "utf8")).toBe(original);
    expect(cli(["--mode", "invalid"]).status).toBe(2);
  });
  test("local qualifier is explicit, pinned and cannot pass by echoing statuses", async () => {
    const source = join(scratch, "fixture-producer.ts"), requested = join(scratch, "fixture-request.json");
    writeFileSync(source, `const request = await Bun.stdin.text(); await Bun.write(${JSON.stringify(requested)}, request); console.log(JSON.stringify({schema:'gptgrep.devbox-selfcheck.qualification-result.v1', ...JSON.parse(request), proofs:[]}));`);
    const value = { ...config(), qualifier: { executable: { path: process.execPath, sha256: await fileHash(process.execPath) }, source: { path: source, sha256: await fileHash(source) }, evidence_root: scratch, timeout_ms: 5000, max_age_ms: 60000 } };
    const args = ["--config", writeConfig(value, "qualifier.json"), "--json"];
    expect(cli(args).status).toBe(1); expect(existsSync(requested)).toBe(false);
    const explicit = cli([...args, "--qualify"]); expect(explicit.status).toBe(1);
    const receipt = JSON.parse(explicit.stdout), request = JSON.parse(readFileSync(requested, "utf8"));
    expect(receipt.qualification.attempted).toBe(true); expect(receipt.overall_status).toBe("UNKNOWN");
    expect(request.request_id).toBe(receipt.receipt_id); expect(request.binding_sha256).toBe(receipt.binding_sha256);
    expect(request.required_probes).toEqual([...PROBES]); expect(existsSync(marker)).toBe(false);
  });
});

describe("bounded immutable input snapshots", () => {
  test("parsing and hashing retain the same bytes after a pathname replacement", () => {
    const path = join(scratch, "snapshot.json"), original = '{"status":"UNKNOWN"}';
    writeFileSync(path, original);
    const snapshot = readSnapshot(path);
    renameSync(path, `${path}.old`); writeFileSync(path, '{"status":"PASSED"}');
    expect(JSON.parse(snapshot.bytes.toString("utf8")).status).toBe("UNKNOWN");
    expect(snapshot.sha256).toBe(sha(original));
    expect(snapshot.sha256).not.toBe(sha(readFileSync(path)));
  });
  test("replacement while reading rejects instead of binding one version to another", () => {
    const path = join(scratch, "racing-proof.json"); writeFileSync(path, '{"status":"UNKNOWN"}');
    let replaced = false;
    expect(() => readSnapshot(path, scratch, (fd, buffer, offset, length, position) => {
      const count = readSync(fd, buffer, offset, length, position);
      if (count && !replaced) { replaced = true; renameSync(path, `${path}.old`); writeFileSync(path, '{"status":"PASSED"}'); }
      return count;
    })).toThrow("Input changed");
    expect(replaced).toBe(true);
  });
  test("growth during reading and over-limit inputs cannot evade the byte bound", () => {
    const path = join(scratch, "growing.json"); writeFileSync(path, "{}");
    let grew = false;
    expect(() => readSnapshot(path, undefined, (fd, buffer, offset, length, position) => {
      const count = readSync(fd, buffer, offset, length, position);
      if (count && !grew) { grew = true; appendFileSync(path, " ".repeat(256 * 1024)); }
      return count;
    })).toThrow("Input changed");
    expect(grew).toBe(true);
    expect(() => readSnapshot(path)).toThrow("bounded physical regular file");
  });
  test("same-inode same-length changes during reading are rejected", () => {
    const path = join(scratch, "in-place.json"); writeFileSync(path, '{"value":"AAAA"}');
    let changed = false;
    expect(() => readSnapshot(path, undefined, (fd, buffer, offset, length, position) => {
      const count = readSync(fd, buffer, offset, length, position);
      if (count && !changed) { changed = true; writeFileSync(path, '{"value":"BBBB"}'); }
      return count;
    })).toThrow("Input changed");
    expect(changed).toBe(true);
  });
});

describe("qualification transport validation, not live acceptance", () => {
  const evidenceRoot = join(scratch, "proof-fixtures"); mkdirSync(evidenceRoot);
  const metadata = join(evidenceRoot, "owner-metadata.json"); writeFileSync(metadata, '{"fixture":true}\n');
  function fixture() {
    const now = Date.now();
    const expected = { probe: PROBES[0], request_id: "test-nonce", binding_sha256: "a".repeat(64), producer_sha256: "b".repeat(64), started_ms: now - 10, ended_ms: now, max_age_ms: 60000, evidence_root: evidenceRoot };
    const proof = { schema: "gptgrep.devbox-selfcheck.probe.v1", scope: "live_current_host", probe: PROBES[0], request_id: expected.request_id, binding_sha256: expected.binding_sha256, producer_sha256: expected.producer_sha256, run_id: "fixture-only-not-live-acceptance", started_at: new Date(now - 10).toISOString(), completed_at: new Date(now).toISOString(), expires_at: new Date(now + 50000).toISOString(), status: "PASSED", command_sha256: "c".repeat(64), evidence: { path: "owner-metadata.json", sha256: sha(readFileSync(metadata)) } };
    return { expected, proof };
  }
  test("format fixture has sufficient bindings; UNKNOWN and FAILED remain distinct", async () => {
    const { expected, proof } = fixture();
    expect(await verifyProbe(proof, expected)).toBe("PASSED");
    expect(await verifyProbe({ ...proof, status: "UNKNOWN" }, expected)).toBe("UNKNOWN");
    expect(await verifyProbe({ ...proof, status: "FAILED" }, expected)).toBe("FAILED");
    expect(aggregate(["PASSED", "UNKNOWN"])).toBe("UNKNOWN");
    expect(aggregate(["UNKNOWN", "FAILED"])).toBe("FAILED");
  });
  test("static/fixture scope, replay, source/host/config binding drift and expired proof fail", async () => {
    const { expected, proof } = fixture();
    for (const patch of [{ scope: "fixture" }, { request_id: "old-nonce" }, { binding_sha256: "d".repeat(64) }, { producer_sha256: "d".repeat(64) }, { expires_at: new Date(expected.ended_ms - 1).toISOString() }, { started_at: new Date(expected.started_ms - 5000).toISOString() }, { command_sha256: null }]) {
      await expect(verifyProbe({ ...proof, ...patch }, expected)).rejects.toThrow();
    }
  });
  test("missing, changed and escaping owner evidence is rejected", async () => {
    const { expected, proof } = fixture();
    for (const evidence of [{ path: "missing.json", sha256: proof.evidence.sha256 }, { path: "owner-metadata.json", sha256: "0".repeat(64) }]) await expect(verifyProbe({ ...proof, evidence }, expected)).rejects.toThrow();
    const outside = join(scratch, "outside.json"); writeFileSync(outside, "{}"); symlinkSync(outside, join(evidenceRoot, "escape.json"));
    await expect(verifyProbe({ ...proof, evidence: { path: "escape.json", sha256: sha("{}") } }, expected)).rejects.toThrow();
  });
});
