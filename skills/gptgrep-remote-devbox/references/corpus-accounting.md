# Three-Tier XL-DocBench Corpus Accounting Specification

## 1. Physical Corpus Invariants
The authoritative XL-DocBench dataset adheres to exact physical byte and page invariants verified during intake:
- **RAW PDFs**: Exactly `291` documents, totaling `3,549,154,931` bytes.
- **Prepared MinerU Pages**: Exactly `82,121` Markdown pages (`mineru-4.0.12-pro-2605` profile).
  - Normal pages: `81,055`
  - Zero-byte pages: `1,066`
  - Missing pages: `0`
- **Extracted Page Images**: Exactly `61,589` PNG/JPEG visual figures.
- **Duplicate Document Paths**: `0`.

## 2. Evaluation Denominator & Split Invariants
- **Total Task Denominator**: Exactly `1,345` tasks.
  - Development Split (`dev`): `366` tasks across `29` source groups.
  - Frozen Split (`frozen`): `979` tasks across `116` source groups.
- **Cross-Split Family Leakage**: Exactly `0` overlapping groups and `0` overlapping task IDs.
- **Frozen Anchor**: Document `doc_000236` (source group `source-abac15165fd3de18fbd1`) is strictly frozen and barred from dev exposure.
