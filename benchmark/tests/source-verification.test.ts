import { expect, test } from "bun:test";
import { verifiedSource } from "../src/source-verification";

test("acquired PDF markers do not admit an error-wrapper PDF", () => {
  expect(verifiedSource({ status: "ready", sha256: "a".repeat(64), pageCount: 263, pageCountVerified: false }, 263)).toBe(false);
});

test("source proof must bind exact bytes, expected pages and edition evidence", () => {
  const source = { status: "ready", sha256: "a".repeat(64), pageCount: 263, pageCountVerified: true,
    sourceVerification: { status: "verified", sha256: "a".repeat(64), expectedPageCount: 263, observedPageCount: 263,
      method: "independent PDF parse and public source metadata", identityEvidence: "Original publisher URL; exact released byte count; title and edition match" } };
  expect(verifiedSource(source, 263)).toBe(true);
  expect(verifiedSource(source, 264)).toBe(false);
  expect(verifiedSource({ ...source, sha256: "b".repeat(64) }, 263)).toBe(false);
  expect(verifiedSource({ ...source, sourceVerification: { ...source.sourceVerification, identityEvidence: "" } }, 263)).toBe(false);
  expect(verifiedSource({ ...source, sourceVerification: { ...source.sourceVerification, observedPageCount: 1 } }, 263)).toBe(false);
});
