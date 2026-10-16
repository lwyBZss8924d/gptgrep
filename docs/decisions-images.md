# Decisions image preparation and qualification

The normal Luna Decisions path accepts typed text/image input for multimodal
search and rerank; see [development setup](development.md) for provider setup and
[rich batch composition](rich-batch.md) for the product entrypoints. This page
describes the separate preparation/qualification interface and its exact limits.

`JevClient::prepare_image_qualification` prepares the flat `state` representation
documented in OpenRouter's [Multimodal Decisions guide](https://openrouter.ai/docs/guides/community/multimodal-decisions).
It accepts only the requested model `openai/gpt-6-luna-decisions` and
`https://openrouter.ai/api/alpha/decisions`. It does not send a request.

The state contains the supplied text strings in input order, followed by direct
image objects in image order. For example, the structural shape is:

```json
{
  "model": "openai/gpt-6-luna-decisions",
  "state": [
    "Inspect the attached image.",
    {
      "type": "image_url",
      "image_url": { "url": "data:image/png;base64,<encoded PNG bytes>" }
    }
  ],
  "questions": {
    "visible": { "type": "noul", "instructions": "Is a mark visible?" }
  },
  "provider": { "allow_fallbacks": false }
}
```

This example contains an explanatory placeholder and is not a sendable body.
The implementation encodes exact admitted PNG or JPEG bytes, omits `detail`, and
does not fetch image URLs. It excludes WebP under the existing local admission
policy. Native OpenAI `input_image` parts and nested image objects are different
representations and are not used here.

`PreparedImageQualification` retains the original `DecisionInput` as a local
source sidecar. `source_input()` exposes its source bindings, text and admitted
image metadata; `text_content_sha256(index)` hashes the original text bytes.
Image evidence retains its exact file digest and dimensions. These fields are
not inserted into the provider body as captions or prompts. `request()` exposes
the existing prepared-request size, SHA-256, requested model and question count.
`into_request()` consumes the sidecar and keeps the exact client-bound request;
callers must retain any needed provenance before consuming it. The wrapper has
no `Debug` or serialization implementation.

Preparation requires declared response capacity of at least 1 MiB. Existing
image limits remain four images, 1 MiB compressed bytes and 1,398,104 base64 bytes
per image, eight million pixels per image, 24 million pixels in total and a
32 MiB decoded-buffer limit. The image header is admitted before full decoding.
The qualification route also requires the serialized transport envelope with
text and questions, excluding image objects, to fit in 64 KiB. This is a stricter
local bound that counts JSON escaping, question descriptions and transport
fields. It is checked using borrowed serialization before base64 or body
allocation. The complete request is serialized once through an 8 MiB bounded
writer. These are application limits, not provider capability claims or a
physical memory/disk reservation guarantee. At most 64 questions are admitted;
response parsing retains its existing 1 MiB cap and typed validation.

The prepared body stays bound to the preparing client's identity, endpoint and
requested model. Its execution context and remaining deadline checks are shared
with the existing transport. Submission uses `submit_prepared` on the preparing
client. It makes one attempt unless that client explicitly opted into the bounded
`TransientRetry` (see [retrieval modes](architecture.md#retrieval-modes)), which
repeats the identical bytes once after a received 429, 500, 502, 503 or 504,
within the original deadline and submission budget. There is no provider
fallback, redirect or implicit retry. The product's rich/multimodal paths remove
any attached retry policy; use a client without that policy when a qualification
must stay single-submission.

Documented syntax does not establish actual backend image interpretation,
semantic eligibility, useful answers or a gateway refusal mapping. Unknown or
unsupported response forms continue to fail closed.

The ordinary `prepare_input` method uses this same flat codec. It applies to the
exact Luna Decisions selector `openai/gpt-6-luna-decisions` and its canonical build
`openai/gpt-6-luna-decisions-20261006`. Images are therefore wired into the CLI:

- `gptgrep judge --input FILE --json` accepts typed `input.text` and `input.images`
  with local PNG/JPEG paths.
- `search --multimodal`, `ask --multimodal`, `rerank` and the rich batch
  `search_multimodal` and `rerank` operations send admitted visual evidence
  through the same Luna codec.

Legacy `typesafe/jev-1.13`, other exact models and every `~author/family-latest`
alias are text-only. They keep the `{evidence:[...]}` text object and reject
images in `prepare_input`, before any reservation or submission. The raw
`{state, questions}` route (`prepare_decision`, `decide` and `gptgrep judge` with a
raw `state`) applies the same rule to these selectors. Any object in `state` or
`questions` with an `image_url` or `input_image` member or `type` is refused before
any reservation or submission. Strings are literal text, never image parts. Luna
receives raw JSON unchanged. An alias can
resolve to a later build outside the pinned Luna identities, so it is never
treated as Luna. `prepare_image_qualification` remains the stricter explicit
wrapper: it also requires the exact alpha endpoint and the bare Luna selector.

Implementation and offline loopback checks do not qualify actual image influence.
A live image qualification needs a separately approved phase. That phase must
bind source and body digests, controls, deadline, accounting and complete output
capacity before submission.
