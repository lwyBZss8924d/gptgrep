import { constants } from "node:fs";
import { lstat, mkdir, open, readFile, readdir, unlink } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { readExactJson, verifyPin, type Pin } from "./availability";
import { canonical, digest, physicalPath, requireStorage, sha256 } from "./io";

export const CAMPAIGN_PURPOSES = ["reader", "judge", "verifier", "repair_reader", "repair_judge", "c4_reader", "c4_judge",
  "calibration", "probe", "native_engineer", "agy", "prepare_context"] as const;
export type CampaignPurpose = typeof CAMPAIGN_PURPOSES[number];
export interface CampaignAllowance { maxAdmissions: number; reservedBytesPerAdmission: number; deadlineMs: number }
export interface CampaignEnvelopeV2 {
  schema: "gptgrep.live-campaign-envelope.v2";
  campaignId: string;
  ledgerRoot: string;
  campaignDeadline: string;
  /** Cumulative logical reservations; neither field describes physical disk usage. */
  maxReservedBytes: number;
  baseReservedBytes: number;
  allowances: Record<CampaignPurpose, CampaignAllowance>;
}
export interface CampaignAction {
  experimentId: string;
  actionId: string;
  purpose: CampaignPurpose;
  commandSha256: string;
}
interface Reservation extends CampaignAction {
  schema: "gptgrep.live-reservation.v2";
  campaignId: string;
  envelope: Pin;
  sequence: number;
  previousReservationSha256: string | null;
  reservedAt: string;
  deadline: string;
  deadlineMs: number;
  bytesReserved: number;
  disposition: "charged_before_dispatch";
}
interface JournalEntry { sequence: number; reservation: Pin }

const MAX_RECORDS = 1024, MAX_RECORD_BYTES = 8192, MAX_JOURNAL_BYTES = 1024 * 1024;
const id = (value: unknown): value is string => typeof value === "string" && /^[A-Za-z0-9][A-Za-z0-9_.-]{0,120}$/.test(value);
const hash = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
const integer = (value: unknown, min: number): value is number => Number.isSafeInteger(value) && (value as number) >= min;
const iso = (value: unknown): value is string => typeof value === "string" && Number.isFinite(Date.parse(value)) && new Date(value).toISOString() === value;
function closed(value: any, fields: string[], label: string) {
  if (!value || typeof value !== "object" || Array.isArray(value) || canonical(Object.keys(value).sort()) !== canonical(fields.sort())) throw new Error(`Malformed ${label}`);
}

/** Runtime extensions are validated by their consumer; allowance rows are closed. */
export function validateCampaignEnvelope(value: CampaignEnvelopeV2): void {
  if (!value || value.schema !== "gptgrep.live-campaign-envelope.v2" || !id(value.campaignId) ||
      typeof value.ledgerRoot !== "string" || !isAbsolute(value.ledgerRoot) || resolve(value.ledgerRoot) !== value.ledgerRoot || value.ledgerRoot === "/" ||
      !iso(value.campaignDeadline) || !integer(value.maxReservedBytes, 1) || !integer(value.baseReservedBytes, 0) ||
      value.baseReservedBytes > value.maxReservedBytes) throw new Error("Invalid finite campaign admission envelope");
  closed(value.allowances, [...CAMPAIGN_PURPOSES], "campaign allowance purposes");
  let totalAdmissions = 0;
  for (const purpose of CAMPAIGN_PURPOSES) {
    const row = value.allowances[purpose];
    closed(row, ["maxAdmissions", "reservedBytesPerAdmission", "deadlineMs"], "campaign allowance");
    if (!integer(row.maxAdmissions, 0) || row.maxAdmissions > MAX_RECORDS || !integer(row.reservedBytesPerAdmission, 1) ||
        !integer(row.deadlineMs, 1)) throw new Error("Campaign allowance requires finite counts, bytes and time");
    totalAdmissions += row.maxAdmissions;
  }
  if (totalAdmissions > MAX_RECORDS) throw new Error("Campaign exceeds the bounded admission record count");
}

/** Purpose changes cannot produce a second reservation for the same action identity. */
export function campaignActionPath(envelope: Pick<CampaignEnvelopeV2, "ledgerRoot">, action: Pick<CampaignAction, "experimentId" | "actionId">): string {
  if (!id(action.experimentId) || !id(action.actionId)) throw new Error("Invalid campaign action identity");
  return join(envelope.ledgerRoot, `${digest({ experimentId: action.experimentId, actionId: action.actionId })}.reservation.json`);
}

async function boundedBytes(path: string, maxBytes: number): Promise<Buffer> {
  await physicalPath(path);
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink() || info.size > maxBytes || (info.mode & 0o777) !== 0o600 ||
      process.getuid && info.uid !== process.getuid()) throw new Error("Campaign ledger requires bounded private regular files");
  const bytes = await readFile(path);
  if (bytes.length !== info.size || bytes.length > maxBytes) throw new Error("Campaign ledger changed while reading");
  return bytes;
}
async function syncDirectory(root: string) {
  const directory = await open(root, "r");
  try { await directory.sync(); } finally { await directory.close(); }
}
async function exclusive(path: string, bytes: Uint8Array): Promise<Pin> {
  await requireStorage(path); await physicalPath(path);
  const file = await open(path, "wx", 0o600);
  try { await file.writeFile(bytes); await file.sync(); } finally { await file.close(); }
  await syncDirectory(dirname(path));
  return { path, sha256: sha256(bytes) };
}
function serialized(value: unknown) { return Buffer.from(`${JSON.stringify(value, null, 2)}\n`); }

async function readLedger(envelope: CampaignEnvelopeV2, envelopePin: Pin, names: string[]) {
  const journalPath = join(envelope.ledgerRoot, "journal.jsonl");
  const text = (await boundedBytes(journalPath, MAX_JOURNAL_BYTES)).toString("utf8");
  if (text && !text.endsWith("\n")) throw new Error("Campaign journal has an uncertain partial append");
  const lines = text ? text.slice(0, -1).split("\n") : [];
  if (lines.length > MAX_RECORDS) throw new Error("Campaign journal exceeds its bounded record count");
  const counts = Object.fromEntries(CAMPAIGN_PURPOSES.map(purpose => [purpose, 0])) as Record<CampaignPurpose, number>;
  let reservedBytes = envelope.baseReservedBytes, previous: string | null = null, lastReservedAt = 0;
  const receipts = new Set<string>();
  for (const [index, line] of lines.entries()) {
    if (Buffer.byteLength(line) > MAX_RECORD_BYTES) throw new Error("Oversized campaign journal entry");
    const entry: JournalEntry = JSON.parse(line);
    closed(entry, ["sequence", "reservation"], "campaign journal entry"); closed(entry.reservation, ["path", "sha256"], "campaign receipt pin");
    if (JSON.stringify(entry) !== line || entry.sequence !== index + 1 || !hash(entry.reservation.sha256) ||
        typeof entry.reservation.path !== "string" || dirname(entry.reservation.path) !== envelope.ledgerRoot ||
        !/^[a-f0-9]{64}\.reservation\.json$/.test(basename(entry.reservation.path))) throw new Error("Malformed or reordered campaign journal");
    const bytes = await boundedBytes(entry.reservation.path, MAX_RECORD_BYTES);
    if (sha256(bytes) !== entry.reservation.sha256) throw new Error("Campaign reservation changed");
    const row: Reservation = JSON.parse(bytes.toString("utf8"));
    closed(row, ["schema", "campaignId", "envelope", "sequence", "previousReservationSha256", "experimentId", "actionId", "purpose", "commandSha256",
      "reservedAt", "deadline", "deadlineMs", "bytesReserved", "disposition"], "campaign reservation");
    const allowance = envelope.allowances[row.purpose];
    if (!CAMPAIGN_PURPOSES.includes(row.purpose) || !allowance || row.schema !== "gptgrep.live-reservation.v2" || row.campaignId !== envelope.campaignId ||
        canonical(row.envelope) !== canonical(envelopePin) || row.sequence !== entry.sequence || row.previousReservationSha256 !== previous ||
        !id(row.experimentId) || !id(row.actionId) || !hash(row.commandSha256) || !iso(row.reservedAt) || !iso(row.deadline) ||
        Date.parse(row.reservedAt) < lastReservedAt || Date.parse(row.deadline) !== Date.parse(row.reservedAt) + allowance.deadlineMs ||
        Date.parse(row.deadline) > Date.parse(envelope.campaignDeadline) || row.deadlineMs !== allowance.deadlineMs ||
        row.bytesReserved !== allowance.reservedBytesPerAdmission || row.disposition !== "charged_before_dispatch" ||
        campaignActionPath(envelope, row) !== entry.reservation.path || !bytes.equals(serialized(row)) || receipts.has(entry.reservation.path)) throw new Error("Campaign reservation identity or frozen allowance differs");
    receipts.add(entry.reservation.path); counts[row.purpose]++;
    reservedBytes += row.bytesReserved;
    if (counts[row.purpose] > allowance.maxAdmissions || !Number.isSafeInteger(reservedBytes) || reservedBytes > envelope.maxReservedBytes) throw new Error("Retained campaign journal exceeds its frozen admission allowance");
    previous = entry.reservation.sha256; lastReservedAt = Date.parse(row.reservedAt);
  }
  const actualReceipts = names.filter(name => name.endsWith(".reservation.json")).map(name => join(envelope.ledgerRoot, name));
  if (actualReceipts.length !== receipts.size || actualReceipts.some(path => !receipts.has(path))) throw new Error("Campaign has orphaned or missing reservation evidence; reconciliation required");
  return { counts, reservedBytes, previous, lastReservedAt, length: lines.length, journalBytes: Buffer.byteLength(text) };
}

/** Read-only verification of an already charged action against the complete durable journal. */
export async function verifyCampaignAction(envelopePin: Pin, action: CampaignAction): Promise<{ reservation: Pin; row: Reservation }> {
  await verifyPin(envelopePin);
  const envelope = await readExactJson<CampaignEnvelopeV2>(envelopePin.path); validateCampaignEnvelope(envelope);
  closed(action, ["experimentId", "actionId", "purpose", "commandSha256"], "campaign action");
  const names = await readdir(envelope.ledgerRoot);
  if (names.includes("reservation.lock")) throw new Error("Campaign admission is still in flight; do not start an unverified action");
  if (names.length > MAX_RECORDS + 2 || names.some(name => !["envelope.json", "journal.jsonl"].includes(name) && !/^[a-f0-9]{64}\.reservation\.json$/.test(name))) throw new Error("Unknown or oversized campaign journal contents");
  const selected = await boundedBytes(join(envelope.ledgerRoot, "envelope.json"), MAX_RECORD_BYTES);
  if (!selected.equals(serialized(envelopePin))) throw new Error("Charged action belongs to another frozen campaign");
  await readLedger(envelope, envelopePin, names);
  const path = campaignActionPath(envelope, action), bytes = await boundedBytes(path, MAX_RECORD_BYTES), row: Reservation = JSON.parse(bytes.toString("utf8"));
  if (Object.entries(action).some(([key, value]) => row[key as keyof CampaignAction] !== value)) throw new Error("Charged action differs from the exact command and purpose");
  if (canonical(names.sort()) !== canonical((await readdir(envelope.ledgerRoot)).sort())) throw new Error("Campaign changed during read-only action verification");
  return { reservation: { path, sha256: sha256(bytes) }, row };
}

/**
 * Charge before dispatch and never refund. All front doors share the same frozen pin.
 * A stale lock, partial append or orphaned receipt requires explicit reconciliation.
 * The ledger's parent must already be durably provisioned by the storage owner.
 * This enforces admission counts and cumulative byte reservations, not a disk quota.
 */
export async function reserveCampaignAction(envelopePin: Pin, action: CampaignAction): Promise<Pin> {
  const envelopeInfo = await lstat(envelopePin.path);
  if (!envelopeInfo.isFile() || envelopeInfo.isSymbolicLink() || envelopeInfo.size > 64 * 1024) throw new Error("Campaign envelope must be a bounded regular file");
  await verifyPin(envelopePin);
  envelopePin = { path: envelopePin.path, sha256: envelopePin.sha256 };
  const envelope = await readExactJson<CampaignEnvelopeV2>(envelopePin.path); validateCampaignEnvelope(envelope);
  await verifyPin(envelopePin);
  closed(action, ["experimentId", "actionId", "purpose", "commandSha256"], "campaign action");
  if (!id(action.experimentId) || !id(action.actionId) || !CAMPAIGN_PURPOSES.includes(action.purpose) || !hash(action.commandSha256)) throw new Error("Invalid campaign action identity or purpose");
  const allowance = envelope.allowances[action.purpose], receiptPath = campaignActionPath(envelope, action), lockPath = join(envelope.ledgerRoot, "reservation.lock");
  await requireStorage(envelope.ledgerRoot, lockPath); await physicalPath(envelope.ledgerRoot);
  const parent = dirname(envelope.ledgerRoot);
  if (!(await lstat(parent)).isDirectory()) throw new Error("Campaign ledger parent must be provisioned before admission");
  try { await mkdir(envelope.ledgerRoot); } catch (error: any) { if (error.code !== "EEXIST") throw error; }
  if (!(await lstat(envelope.ledgerRoot)).isDirectory()) throw new Error("Campaign ledger root must be a directory");
  // Every contender persists the new link, including one that lost the mkdir race.
  await syncDirectory(parent);
  const lock = await open(lockPath, "wx", 0o600);
  try {
    await lock.writeFile(`${JSON.stringify({ pid: process.pid, campaignId: envelope.campaignId })}\n`); await lock.sync(); await syncDirectory(envelope.ledgerRoot);
    let names = await readdir(envelope.ledgerRoot);
    if (names.length > MAX_RECORDS + 3 || names.some(name => !["reservation.lock", "envelope.json", "journal.jsonl"].includes(name) && !/^[a-f0-9]{64}\.reservation\.json$/.test(name))) throw new Error("Unknown or oversized campaign ledger contents");
    const selectedPath = join(envelope.ledgerRoot, "envelope.json"), journalPath = join(envelope.ledgerRoot, "journal.jsonl");
    if (!names.includes("envelope.json") && names.length === 1) {
      await exclusive(selectedPath, serialized(envelopePin)); await exclusive(journalPath, Buffer.alloc(0));
      names = await readdir(envelope.ledgerRoot);
    }
    const selected = await boundedBytes(selectedPath, MAX_RECORD_BYTES);
    if (!selected.equals(serialized(envelopePin))) throw new Error("Campaign envelope changed; experiment names cannot reset admission allowances");
    const state = await readLedger(envelope, envelopePin, names);
    if (names.includes(receiptPath.slice(envelope.ledgerRoot.length + 1))) throw new Error("Campaign action is already charged; automatic retry is forbidden");
    if (state.counts[action.purpose] >= allowance.maxAdmissions) throw new Error(`Campaign ${action.purpose} admission allowance exhausted`);
    const nextReservedBytes = state.reservedBytes + allowance.reservedBytesPerAdmission;
    if (!Number.isSafeInteger(nextReservedBytes) || nextReservedBytes > envelope.maxReservedBytes) throw new Error("Campaign cumulative byte reservation exhausted");
    const now = Date.now(), deadline = now + allowance.deadlineMs;
    if (!Number.isSafeInteger(deadline) || now < state.lastReservedAt || deadline > Date.parse(envelope.campaignDeadline)) throw new Error("Campaign deadline cannot admit another complete action");
    const row: Reservation = { schema: "gptgrep.live-reservation.v2", campaignId: envelope.campaignId, envelope: envelopePin,
      sequence: state.length + 1, previousReservationSha256: state.previous, ...action, reservedAt: new Date(now).toISOString(), deadline: new Date(deadline).toISOString(),
      deadlineMs: allowance.deadlineMs, bytesReserved: allowance.reservedBytesPerAdmission, disposition: "charged_before_dispatch" };
    const bytes = serialized(row);
    if (bytes.length > MAX_RECORD_BYTES) throw new Error("Campaign reservation exceeds its byte bound");
    const receipt = { path: receiptPath, sha256: sha256(bytes) };
    const line = `${JSON.stringify({ sequence: row.sequence, reservation: receipt })}\n`;
    if (state.journalBytes + Buffer.byteLength(line) > MAX_JOURNAL_BYTES) throw new Error("Campaign journal exceeds its byte bound");
    // A crash between these writes leaves an orphan and blocks subsequent admissions.
    await exclusive(receiptPath, bytes);
    await requireStorage(journalPath);
    const journal = await open(journalPath, constants.O_WRONLY | constants.O_APPEND | constants.O_NOFOLLOW);
    try { await journal.writeFile(line); await journal.sync(); } finally { await journal.close(); }
    return receipt;
  } finally {
    await lock.close(); await requireStorage(lockPath); await unlink(lockPath); await syncDirectory(envelope.ledgerRoot);
  }
}
