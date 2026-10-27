import { expect, test } from "bun:test";
import { verifiedSource } from "../src/source-verification";

test("acquired PDF markers do not admit an error-wrapper PDF", () => {
  expect(verifiedSource({ status: "ready", sha256: "a".repeat(64), pageCount: 263, pageCountVerified: false }, 263)).toBe(false);
});

test("source proof must bind exact bytes, expected pages and edition evidence", () => {
  const source = { status: "ready", sha256: "a".repeat(64), pageCount: 263, pageCountVerified: true,
    sourceVerification: { status: "verified", sha256: "a".repeat(64), expectedPageCount: 263, observedPageCount: 263,
      method: "independent PDF parse and public source metadata", identityEvidence: "Original publisher URL; exact released byte count; title and edition match",
      identity: { status: "verified", basis: "publisher_release_metadata", expectedIdentitySha256: "b".repeat(64), sourceUrl: "https://example.invalid/document.pdf",
        evidence: [{ path: "/synthetic/source.json", sha256: "c".repeat(64) }], review: null } } };
  expect(verifiedSource(source, 263)).toBe(true);
  expect(verifiedSource(source, 264)).toBe(false);
  expect(verifiedSource({ ...source, sha256: "b".repeat(64) }, 263)).toBe(false);
  expect(verifiedSource({ ...source, sourceVerification: { ...source.sourceVerification, identityEvidence: "" } }, 263)).toBe(false);
  expect(verifiedSource({ ...source, sourceVerification: { ...source.sourceVerification, observedPageCount: 1 } }, 263)).toBe(false);
  const withoutIdentity = structuredClone(source); delete (withoutIdentity.sourceVerification as any).identity;
  expect(verifiedSource(withoutIdentity, 263)).toBe(false);
  const held = structuredClone(source); held.sourceVerification.identity.status = "held";
  expect(verifiedSource(held, 263)).toBe(false);
  const equivalent = structuredClone(source); equivalent.sourceVerification.identity.basis = "independent_equivalence_review";
  expect(verifiedSource(equivalent, 263)).toBe(false);
  (equivalent.sourceVerification.identity as any).review = { path: "/synthetic/review.json", sha256: "d".repeat(64) };
  expect(verifiedSource(equivalent, 263)).toBe(true);
});
