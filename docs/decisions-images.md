# Experimental Decisions image preparation

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
with the existing transport. Submission uses the existing single-attempt path,
without provider fallback, redirects or implicit retries.

Documented syntax does not establish actual backend image interpretation,
semantic eligibility, useful answers or a gateway refusal mapping. Unknown or
unsupported response forms continue to fail closed. The ordinary
`prepare_input` method still rejects image inputs. This experimental API does
not wire images into the CLI, select a new default model, promote the ordinary
image gate, or authorize a live qualification attempt. A separately admitted
phase must bind source and body digests, controls, deadline, accounting and
complete output capacity before submission.
