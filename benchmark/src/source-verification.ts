export interface SourceIdentityPin { path: string; sha256: string; }
export interface SourceIdentity {
  status: "verified" | "held" | "unknown";
  basis: "publisher_release_metadata" | "independent_equivalence_review";
  expectedIdentitySha256: string;
  sourceUrl: string;
  evidence: SourceIdentityPin[];
  review: SourceIdentityPin | null;
}

function isPin(value: any): boolean {
  return value !== null && typeof value === "object" && !Array.isArray(value) &&
    Object.keys(value).sort().join("\0") === "path\0sha256" &&
    typeof value.path === "string" && value.path.trim().length > 0 && /^[a-f0-9]{64}$/.test(value.sha256);
}

export function validSourceIdentity(value: any): value is SourceIdentity {
  if (!value || typeof value !== "object" || Array.isArray(value) ||
      Object.keys(value).sort().join("\0") !== ["status", "basis", "expectedIdentitySha256", "sourceUrl", "evidence", "review"].sort().join("\0") ||
      !["verified", "held", "unknown"].includes(value.status) ||
      !["publisher_release_metadata", "independent_equivalence_review"].includes(value.basis) ||
      !/^[a-f0-9]{64}$/.test(value.expectedIdentitySha256) ||
      typeof value.sourceUrl !== "string" || !Array.isArray(value.evidence) || !value.evidence.length ||
      !value.evidence.every(isPin) || value.review !== null && !isPin(value.review)) return false;
  try { if (!["http:", "https:"].includes(new URL(value.sourceUrl).protocol)) return false; } catch { return false; }
  return value.basis !== "independent_equivalence_review" || value.review !== null;
}

/** Physical acquisition and prose alone cannot establish released-source identity. */
export function verifiedSource(record: any, expectedPageCount: number): boolean {
  const proof = record?.sourceVerification;
  return record?.status === "ready" && record.pageCountVerified === true &&
    Number.isSafeInteger(expectedPageCount) && expectedPageCount > 0 &&
    record.pageCount === expectedPageCount &&
    proof?.status === "verified" && proof.sha256 === record.sha256 &&
    /^[a-f0-9]{64}$/.test(record.sha256) &&
    proof.expectedPageCount === expectedPageCount && proof.observedPageCount === expectedPageCount &&
    typeof proof.method === "string" && proof.method.length > 0 &&
    typeof proof.identityEvidence === "string" && proof.identityEvidence.length > 0 &&
    validSourceIdentity(proof.identity) && proof.identity.status === "verified";
}
