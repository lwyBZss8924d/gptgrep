# Native page metadata

`inspect_native_metadata` reads structured metadata for one physical page from a
published rich generation. It runs locally through the ordinary rich batch
interface and the CLI wrapper. It does not call a model or grant citation rights.

The JavaScript wrapper serializes signed zero as `0`. This is a JSON numeric
representation change, not a change to geometric values, retained artifact
bytes, source bindings or digests. All other numeric values retain their JSON
value; strict validation of stored native metadata remains unchanged.

First use `inspect_pages` to discover the page's `metadata_artifact.id`:

```json
{
  "schema_version": "gptgrep.batch.rich.request.v1",
  "generation": "CURRENT",
  "operations": [
    {"id": "pages", "op": "inspect_pages", "document": "report.pdf", "fields": ["page", "metadata_artifact"], "page_size": 10}
  ]
}
```

Pass this request on stdin to `gptgrep batch --rich --root DOCUMENTS --input -`.
For subsequent requests, copy the complete `generation` object from the response
into the request's `generation` field. This keeps navigation on the same
generation if another process publishes a new one.

Read the discovered metadata artifact with a separate operation:

```json
{
  "id": "objects",
  "op": "inspect_native_metadata",
  "artifact_id": "COPY_PAGE_METADATA_ARTIFACT_ID",
  "collection": "object_summaries",
  "fields": ["geometry", "content_bounds", "counts"],
  "item_fields": ["id", "kind", "segment_count", "page_bounds"],
  "page_size": 20,
  "max_output_bytes": 16384
}
```

Replace the placeholder with the returned artifact ID and place the operation
inside a rich batch request. A document-wide metadata ID or text artifact ID is
not a page metadata ID. Dependent requests are composed by the caller; one
operation cannot refer to another operation's result within the same batch.

| Collection | Rows |
| --- | --- |
| `object_summaries` | Page objects in source traversal order, including nested forms |
| `path_segments` | Segments for the `object_id` of a path returned by `object_summaries` |
| `image_geometry` | Image dimensions and placement on the physical page |
| `native_characters` | Retained native character, font and geometry observations |
| `native_words` | Words derived from the native character stream |

Scalar `fields` select `geometry`, `content_bounds`, `availability` and `counts`.
`item_fields` select whole fields from the chosen collection. Field names are
closed; use `gptgrep --schema` for the current lists. Empty `item_fields` includes
all row fields. Empty scalar `fields` includes geometry, availability and counts;
request `content_bounds` explicitly. `object_id` is required only for
`path_segments`.

Continue with the returned item's `coverage.cursor`, keeping the exact generation,
artifact, collection, object, field selections, page size and byte limit. The
batch's reserved output share also limits the page that can be returned. A
complete row and continuation must fit; if even the next row cannot fit an empty
page, the operation produces `output_budget_insufficient`. Otherwise it returns
the rows that fit and a continuation, without truncating rows. `page_size` is
1–1000, and an operation's output limit is at most
1 MiB within the batch's shared 1 MiB limit.

The result reports `metadata_only: true` and `citation_authority: false`. It
verifies the selected source, prepared document and metadata artifact before
returning. A source change or shared integrity failure is explicit; a cursor does
not make stale data valid. Reading these structures does not count as delivering
text or an image to a reasoning model.

Geometry retains the source-to-page transform, object ancestry and native
observations. Native bounds can be signed, extend beyond the CropBox or have zero
area. Content bounds describe the union of native top-level geometric extents;
they do not describe clipped visible pixels. Unsupported paint or pixel
orientation remains an explicit availability state. Raw path coordinates,
transformed points and native bounds have different meanings and should not be
substituted for one another.

The v7 recipe supplies complete graphics metadata. Retained v6 generations can
still expose their qualified characters and words; requests for unavailable
graphics return an explicit `invalid_operation` error asking for a rebuild.
Older generations remain on disk. The reader
admits at most 4 MiB per metadata artifact and accounts for retained snapshots,
owned response values and freshness scratch under a shared 256 MiB working
allowance. These are logical admission limits, not a process RSS limit.


## Rich reader tools

Rich text and multimodal readers expose the same local metadata inspection tool,
`gptgrep_inspect_native_metadata`. First call `gptgrep_inspect_artifact` for a
document and copy the selected page's `metadata_artifact.id`. Pass that ID as
`artifact_id`, for example:

```json
{
  "artifact_id": "COPY_PAGE_METADATA_ARTIFACT_ID",
  "collection": "object_summaries",
  "fields": ["geometry", "content_bounds", "counts"],
  "item_fields": ["id", "kind", "segment_count", "page_bounds"],
  "page_size": 20,
  "max_output_bytes": 16384
}
```

The host selects its pinned generation. A document-scoped reader accepts only
page metadata belonging to that document. A document metadata ID, text artifact
ID, unknown ID or another document's page metadata ID cannot select a page.
Source and artifact freshness checks still apply to every request.

The five collections, closed field selections and path `object_id` rules above
also apply to this tool. The host accepts `page_size` from 1 to 1000 (default 100)
and `max_output_bytes` from 1 to 16384 (default 16384). This output setting bounds
the core metadata item; the host separately bounds the complete response and RPC
frame. The complete metadata argument object is limited to 20 KiB, including
its cursor. Other reader tools retain their existing limits.

Continue with the returned batch item's `coverage.cursor`, keeping the artifact,
collection, object, field selections, page size and output limit unchanged. The
host preserves complete rows and returns an explicit error if the next whole
row and its required metadata cannot fit. It does not shorten rows or accept a
cursor for a different selection.

Metadata is useful for navigation and understanding native geometry. Character
and word observations, paths, placement bounds and content bounds remain
metadata. Their receipt issues no text or image citation ID. Text citations
require the normal `gptgrep_read_artifact` delivery; image citations in a
multimodal reader require `gptgrep_render` delivery. Signed native bounds are
not image crop coordinates or proof of visible pixels.

This local tool consumes the reader's existing tool-call and transport budgets
and original deadline. Inspecting metadata itself makes no provider request.
The response retains its shared metadata admission while the host serializes
and sends it, including write/flush and subsequent freshness checks.
