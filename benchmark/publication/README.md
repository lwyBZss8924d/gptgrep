# Public reader Parquet

This uv project pins PyArrow for the closed public reader projection. Its Python
adapter writes explicit column schemas with `store_schema=False`, decodes actual
Parquet data, and rejects duplicate physical column paths, unexpected columns,
external column references and nonempty footer, schema or nested field metadata.
Decoded rows must retain canonical JSONL parity. Input, expanded row and output
limits bound the supported publication files.

Run every installation and qualification through the repository's admitted
development-storage launcher. `uv sync --locked --no-build --no-install-project`
uses an explicitly selected external Python 3.13 interpreter and run environment.
The runtime receipt records that interpreter and hashes the installed package
files; subsequent reads and writes verify the same identity. Do not point this
adapter at an unqualified environment.

`qualify.ts --python <absolute-python> --output <new-admitted-directory>` runs
synthetic format contract tests, records the runtime and source pins, and checks
actual JSONL/Parquet parity through the Bun publication verifier. These fixtures
are engineering checks and do not count as benchmark results.

`stage.ts` accepts a qualified receipt and a public-only JSONL staging manifest:

```sh
bun benchmark/publication/stage.ts \
  --qualification <qualification.json> --qualification-sha256 <sha256> \
  --source-root <public-source-directory> \
  --source-manifest <public-manifest.json> --source-manifest-sha256 <sha256> \
  --output <new-admitted-directory>
```

It first verifies the source allowlist, copies the safe files into a new `public/`
directory, writes all three Parquet partitions and decodes every row for parity.
The manifest, runtime and verification receipts stay outside `public/`. It does
not upload anything. The publication caller must retain independent acceptance
and reverify the exact staged manifest immediately before a remote upload.
