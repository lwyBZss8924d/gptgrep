import { expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CAMPAIGN_PURPOSES, campaignActionPath, reserveCampaignAction, validateCampaignEnvelope, type CampaignAction, type CampaignEnvelopeV2 } from "../src/campaign";
import { fileHash, sha256 } from "../src/io";

async function fixture(change: Partial<CampaignEnvelopeV2> = {}) {
  const root = await mkdtemp(join(tmpdir(), "campaign-admission-"));
  const previous = [process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING, process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST];
  const write = async (name: string, value: unknown) => {
    const path = join(root, name); await writeFile(path, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
    return { path, sha256: await fileHash(path) };
  };
  const binding = await write("test-binding.json", { targets: { output: root } });
  process.env.ENVIRONMENTS_HARNESS_STORAGE_BINDING = binding.path; process.env.ENVIRONMENTS_HARNESS_STORAGE_DIGEST = binding.sha256;
  const envelope: CampaignEnvelopeV2 = { schema: "gptgrep.live-campaign-envelope.v2", campaignId: "campaign", ledgerRoot: join(root, "ledger"),
    campaignDeadline: new Date(Date.now() + 60_000).toISOString(), maxReservedBytes: 10_000, baseReservedBytes: 100,
    allowances: Object.fromEntries(CAMPAIGN_PURPOSES.map(purpose => [purpose, { maxAdmissions: 2, reservedBytesPerAdmission: 100, deadlineMs: 1000 }])) as CampaignEnvelopeV2["allowances"], ...change };
  const envelopePin = await write("envelope.json", envelope);
  const action = (actionId: string, purpose: CampaignAction["purpose"] = "reader", experimentId = "experiment"): CampaignAction =>
    ({ actionId, purpose, experimentId, commandSha256: sha256("exact reviewed command") });
  const cleanup = async () => {
    for (const [index, key] of ["ENVIRONMENTS_HARNESS_STORAGE_BINDING", "ENVIRONMENTS_HARNESS_STORAGE_DIGEST"].entries()) {
      if (previous[index] === undefined) delete process.env[key]; else process.env[key] = previous[index];
    }
    await rm(root, { recursive: true, force: true });
  };
  return { root, write, envelope, envelopePin, action, cleanup };
}

// These fixtures test logical admission accounting under the outer admitted test run.
// They do not demonstrate physical disk quotas, provider limits or owner runtime cleanup.
test("one campaign charges every front door across experiments without refunds or automatic retries", async () => {
  const f = await fixture();
  try {
    const first = await reserveCampaignAction(f.envelopePin, f.action("first"));
    expect(first.path).toBe(campaignActionPath(f.envelope, f.action("first")));
    const row = JSON.parse(await readFile(first.path, "utf8"));
    expect(row.disposition).toBe("charged_before_dispatch"); expect(row.bytesReserved).toBe(100);
    expect(Date.parse(row.deadline) - Date.parse(row.reservedAt)).toBe(1000);
    await expect(reserveCampaignAction(f.envelopePin, f.action("first", "judge"))).rejects.toThrow("already charged");
    await reserveCampaignAction(f.envelopePin, f.action("second", "reader", "other-experiment"));
    await expect(reserveCampaignAction(f.envelopePin, f.action("third", "reader", "new-experiment"))).rejects.toThrow("allowance exhausted");
    for (const purpose of CAMPAIGN_PURPOSES.filter(purpose => purpose !== "reader")) await reserveCampaignAction(f.envelopePin, f.action(`action-${purpose}`, purpose));
    expect((await readdir(f.envelope.ledgerRoot)).filter(name => name.endsWith(".reservation.json"))).toHaveLength(CAMPAIGN_PURPOSES.length + 1);
  } finally { await f.cleanup(); }
});

test("aggregate byte reservations include the frozen base and unequal PREPARE and reader allowances", async () => {
  const f = await fixture({ maxReservedBytes: 450 });
  try {
    f.envelope.allowances.prepare_context.reservedBytesPerAdmission = 250;
    const pin = await f.write("selected.json", f.envelope);
    await reserveCampaignAction(pin, f.action("prepare", "prepare_context"));
    await reserveCampaignAction(pin, f.action("read"));
    await expect(reserveCampaignAction(pin, f.action("judge", "judge"))).rejects.toThrow("cumulative byte reservation exhausted");
    const entries = (await readFile(join(f.envelope.ledgerRoot, "journal.jsonl"), "utf8")).trim().split("\n");
    expect(entries).toHaveLength(2);
  } finally { await f.cleanup(); }
});

test("offline verifier work consumes its own admission allowance and the shared logical byte budget", async () => {
  const f = await fixture({ maxReservedBytes: 400 });
  try {
    f.envelope.allowances.verifier = { maxAdmissions: 1, reservedBytesPerAdmission: 200, deadlineMs: 500 };
    const pin = await f.write("verifier-envelope.json", f.envelope);
    const action = { ...f.action("verify-first", "verifier"), commandSha256: sha256("reviewed offline owner verifier command") };
    const receipt = await reserveCampaignAction(pin, action);
    const row = JSON.parse(await readFile(receipt.path, "utf8"));
    expect(row.purpose).toBe("verifier"); expect(row.commandSha256).toBe(action.commandSha256); expect(row.deadlineMs).toBe(500);
    await expect(reserveCampaignAction(pin, f.action("verify-second", "verifier", "new-experiment"))).rejects.toThrow("verifier admission allowance exhausted");
    await reserveCampaignAction(pin, f.action("reader"));
    await expect(reserveCampaignAction(pin, f.action("judge", "judge"))).rejects.toThrow("cumulative byte reservation exhausted");
  } finally { await f.cleanup(); }
});

test("equivalent envelope pins retain identity regardless of object property order", async () => {
  const f = await fixture();
  try {
    await mkdir(join(f.root, "new", "nested"), { recursive: true });
    const pin = await f.write("nested-envelope.json", { ...f.envelope, ledgerRoot: join(f.root, "new", "nested", "ledger") });
    const reordered = { sha256: pin.sha256, path: pin.path };
    await reserveCampaignAction(reordered, f.action("first"));
    await reserveCampaignAction(pin, f.action("second"));
    await expect(reserveCampaignAction(reordered, f.action("third"))).rejects.toThrow("allowance exhausted");
  } finally { await f.cleanup(); }
});

test("admission cannot create a missing owner-provisioned parent hierarchy", async () => {
  const f = await fixture();
  try {
    const pin = await f.write("missing-parent.json", { ...f.envelope, ledgerRoot: join(f.root, "missing", "ledger") });
    await expect(reserveCampaignAction(pin, f.action("first"))).rejects.toThrow();
    expect(await readdir(f.root)).not.toContain("missing");
  } finally { await f.cleanup(); }
});

test("replacement envelopes and extra or malformed allowance purposes cannot reset admission", async () => {
  const f = await fixture();
  try {
    await reserveCampaignAction(f.envelopePin, f.action("first"));
    const replacement = await f.write("replacement.json", { ...f.envelope, campaignId: "new-campaign" });
    await expect(reserveCampaignAction(replacement, f.action("second"))).rejects.toThrow("cannot reset");
    for (const changed of [
      { ...f.envelope, allowances: { ...f.envelope.allowances, undeclared: { maxAdmissions: 1, reservedBytesPerAdmission: 1, deadlineMs: 1 } } },
      { ...f.envelope, allowances: { ...f.envelope.allowances, probe: { ...f.envelope.allowances.probe, maxAdmissions: Infinity } } },
      { ...f.envelope, allowances: { ...f.envelope.allowances, probe: { ...f.envelope.allowances.probe, refundable: true } } },
      { ...f.envelope, maxReservedBytes: Number.MAX_SAFE_INTEGER + 1 },
    ]) expect(() => validateCampaignEnvelope(changed as any)).toThrow();
    expect(() => validateCampaignEnvelope({ ...f.envelope, maxTraceBytes: 1024 } as CampaignEnvelopeV2)).not.toThrow();
  } finally { await f.cleanup(); }
});

test("action deadlines and disabled purposes deny before creating a receipt", async () => {
  const f = await fixture({ campaignDeadline: new Date(0).toISOString() });
  try {
    await expect(reserveCampaignAction(f.envelopePin, f.action("expired"))).rejects.toThrow("deadline");
    expect((await readdir(f.envelope.ledgerRoot)).filter(name => name.endsWith(".reservation.json"))).toHaveLength(0);
    const fresh = { ...f.envelope, ledgerRoot: join(f.root, "disabled-ledger"), campaignDeadline: new Date(Date.now() + 60_000).toISOString(),
      allowances: { ...f.envelope.allowances, probe: { ...f.envelope.allowances.probe, maxAdmissions: 0 } } };
    const pin = await f.write("disabled.json", fresh);
    await expect(reserveCampaignAction(pin, f.action("probe", "probe"))).rejects.toThrow("allowance exhausted");
  } finally { await f.cleanup(); }
});

test("edited or deleted reservation receipts remain blocking even with a new experiment name", async () => {
  const f = await fixture();
  try {
    const first = await reserveCampaignAction(f.envelopePin, f.action("first"));
    const original = await readFile(first.path, "utf8"), row = JSON.parse(original);
    await writeFile(first.path, JSON.stringify({ ...row, bytesReserved: 0 }));
    await expect(reserveCampaignAction(f.envelopePin, f.action("second", "judge", "new-experiment"))).rejects.toThrow("changed");
    await writeFile(first.path, original); await rm(first.path);
    await expect(reserveCampaignAction(f.envelopePin, f.action("third", "judge", "another-experiment"))).rejects.toThrow();
  } finally { await f.cleanup(); }
});

test("journal edits cannot relabel a fixed allowance or reference a file outside the ledger", async () => {
  const f = await fixture();
  try {
    const first = await reserveCampaignAction(f.envelopePin, f.action("first")), journal = join(f.envelope.ledgerRoot, "journal.jsonl");
    const row = JSON.parse(await readFile(first.path, "utf8")); row.deadlineMs = 1;
    await writeFile(first.path, `${JSON.stringify(row, null, 2)}\n`);
    await writeFile(journal, `${JSON.stringify({ sequence: 1, reservation: { ...first, sha256: await fileHash(first.path) } })}\n`);
    await expect(reserveCampaignAction(f.envelopePin, f.action("second"))).rejects.toThrow("frozen allowance");
    await writeFile(journal, `${JSON.stringify({ sequence: 1, reservation: f.envelopePin })}\n`);
    await expect(reserveCampaignAction(f.envelopePin, f.action("third"))).rejects.toThrow("Malformed or reordered");
  } finally { await f.cleanup(); }
});

test("orphaned receipts, partial journals and stale mutexes require reconciliation", async () => {
  const f = await fixture();
  try {
    await reserveCampaignAction(f.envelopePin, f.action("first"));
    const orphan = campaignActionPath(f.envelope, f.action("orphan"));
    await writeFile(orphan, "{}", { mode: 0o600 });
    await expect(reserveCampaignAction(f.envelopePin, f.action("second"))).rejects.toThrow("orphaned");
    await rm(orphan);
    const journal = join(f.envelope.ledgerRoot, "journal.jsonl"), original = await readFile(journal, "utf8");
    await writeFile(journal, `${original}{`);
    await expect(reserveCampaignAction(f.envelopePin, f.action("third"))).rejects.toThrow("partial append");
    await writeFile(journal, original);
    await writeFile(join(f.envelope.ledgerRoot, "reservation.lock"), "retained uncertain owner", { mode: 0o600 });
    await expect(reserveCampaignAction(f.envelopePin, f.action("fourth"))).rejects.toThrow();
    expect(await readFile(join(f.envelope.ledgerRoot, "reservation.lock"), "utf8")).toBe("retained uncertain owner");
  } finally { await f.cleanup(); }
});

test("concurrent callers cannot both consume a one-action allowance", async () => {
  const f = await fixture();
  try {
    f.envelope.allowances.reader.maxAdmissions = 1;
    const pin = await f.write("single.json", f.envelope);
    const results = await Promise.allSettled([reserveCampaignAction(pin, f.action("a")), reserveCampaignAction(pin, f.action("b"))]);
    expect(results.filter(result => result.status === "fulfilled")).toHaveLength(1);
    expect(results.filter(result => result.status === "rejected")).toHaveLength(1);
    await expect(reserveCampaignAction(pin, f.action("c"))).rejects.toThrow("allowance exhausted");
  } finally { await f.cleanup(); }
});
