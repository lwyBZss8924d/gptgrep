/** Acquisition alone does not establish that a returned PDF is the released source. */
export function verifiedSource(record: any, expectedPageCount: number): boolean {
  const proof = record?.sourceVerification;
  return record?.status === "ready" && record.pageCountVerified === true &&
    Number.isSafeInteger(expectedPageCount) && expectedPageCount > 0 &&
    record.pageCount === expectedPageCount &&
    proof?.status === "verified" && proof.sha256 === record.sha256 &&
    /^[a-f0-9]{64}$/.test(record.sha256) &&
    proof.expectedPageCount === expectedPageCount && proof.observedPageCount === expectedPageCount &&
    typeof proof.method === "string" && proof.method.length > 0 &&
    typeof proof.identityEvidence === "string" && proof.identityEvidence.length > 0;
}
