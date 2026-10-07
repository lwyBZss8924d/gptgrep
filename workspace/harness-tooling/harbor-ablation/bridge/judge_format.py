"""Pure verifier-only formatting. Source literals are parsed, never imported."""
import ast
import hashlib
import json
import math
import struct
import sys

MAX_SOURCE = 1024 * 1024
MAX_ARTIFACT = 64 * 1024
MAX_OUTPUT = 256 * 1024
WANTED = {"PROMPT", "SCHEMA", "MAX_RESPONSE_CHARS", "MODEL", "EFFORT"}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            raise ValueError("duplicate_json_field")
        result[key] = value
    return result


def json_value(data):
    return json.loads(data, object_pairs_hook=pairs,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite_json")))


def schema_valid(schema):
    if not isinstance(schema, dict) or set(schema) != {"type", "properties", "required", "additionalProperties"}:
        raise ValueError("unsupported_original_schema")
    if schema["type"] != "object" or schema["additionalProperties"] is not False \
            or not isinstance(schema["required"], list) or len(schema["required"]) != 3 \
            or set(schema["required"]) != {"equivalent", "abstained", "reason"}:
        raise ValueError("unsupported_original_schema")
    properties = schema["properties"]
    if not isinstance(properties, dict) or set(properties) != {"equivalent", "abstained", "reason"}:
        raise ValueError("unsupported_original_schema")
    for name, kind in (("equivalent", "boolean"), ("abstained", "boolean"), ("reason", "string")):
        prop = properties[name]
        if not isinstance(prop, dict) or set(prop) - {"type", "description"} or prop.get("type") != kind \
                or ("description" in prop and (not isinstance(prop["description"], str) or len(prop["description"]) > 1024)):
            raise ValueError("unsupported_original_schema")


def constants(source):
    tree = ast.parse(source.decode("utf-8"))
    if sum(1 for _ in ast.walk(tree)) > 100000:
        raise ValueError("source_ast_complexity")
    found = {}
    for node in tree.body:
        if isinstance(node, ast.Assign):
            selected = [target.id for target in node.targets if isinstance(target, ast.Name) and target.id in WANTED]
            if selected:
                if len(node.targets) != 1 or len(selected) != 1 or selected[0] in found:
                    raise ValueError("ambiguous_original_literal")
                for child in ast.walk(node.value):
                    if isinstance(child, ast.Dict):
                        keys = [ast.literal_eval(key) for key in child.keys]
                        if len(set(keys)) != len(keys):
                            raise ValueError("duplicate_original_literal")
                found[selected[0]] = ast.literal_eval(node.value)
    if set(found) != WANTED or not isinstance(found["PROMPT"], str) \
            or type(found["MAX_RESPONSE_CHARS"]) is not int or not 1 <= found["MAX_RESPONSE_CHARS"] <= 12000 \
            or any(not isinstance(found[name], str) or not found[name] or len(found[name]) > 128 for name in ("MODEL", "EFFORT")):
        raise ValueError("original_literal_contract")
    schema_valid(found["SCHEMA"])
    return found


def finite(value):
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError("nonfinite_reference")
    if isinstance(value, list):
        for item in value:
            finite(item)
    if isinstance(value, dict):
        for item in value.values():
            finite(item)


def format_buffers(source, reference_bytes, answer_bytes):
    if not 0 < len(source) <= MAX_SOURCE or not 0 < len(reference_bytes) <= MAX_ARTIFACT \
            or not 0 < len(answer_bytes) <= MAX_ARTIFACT:
        raise ValueError("input_buffer_limit")
    original = constants(source)
    reference = json_value(reference_bytes)
    answer = json_value(answer_bytes)
    if not isinstance(reference, dict) or set(reference) != {"schema_version", "question", "answer", "answer_format"} \
            or reference["schema_version"] != "gptgrep.judge.reference.v1" \
            or not isinstance(reference["question"], str) or not reference["question"].strip() \
            or len(reference["question"]) > 12000 or not isinstance(reference["answer_format"], str) \
            or not reference["answer_format"] or len(reference["answer_format"]) > 128 or reference["answer"] is None:
        raise ValueError("reference_contract")
    finite(reference["answer"])
    if not isinstance(answer, dict) or set(answer) != {"answer"} or not isinstance(answer["answer"], str) \
            or not answer["answer"].strip():
        raise ValueError("answer_contract")
    response = str(answer["answer"])
    used = response[:original["MAX_RESPONSE_CHARS"]]
    normalized = " ".join(reference["question"].split())
    prompt = original["PROMPT"].format(question=normalized, answer=reference["answer"],
                                        answer_format=reference["answer_format"], response=used)
    schema_bytes = json.dumps(original["SCHEMA"], ensure_ascii=False, sort_keys=True,
                             separators=(",", ":"), allow_nan=False).encode()
    result = {"schema_version": "gptgrep.judge.formatted.v1", "judge_source_sha256": digest(source),
              "reference_sha256": digest(reference_bytes), "answer_sha256": digest(answer_bytes),
              "prompt_template_sha256": digest(original["PROMPT"].encode()),
              "prompt_sha256": digest(prompt.encode()), "verdict_schema_sha256": digest(schema_bytes),
              "prompt": prompt, "verdict_schema": original["SCHEMA"],
              "upstream_model": original["MODEL"], "upstream_effort": original["EFFORT"],
              "max_response_characters": original["MAX_RESPONSE_CHARS"],
              "original_response_characters": len(response), "used_response_characters": len(used),
              "response_truncated": len(response) > len(used)}
    output = json.dumps(result, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()
    if len(output) > MAX_OUTPUT:
        raise ValueError("formatted_output_limit")
    return output


def read_frame(handle, limit):
    header = handle.read(8)
    if len(header) != 8:
        raise ValueError("frame_header")
    length = struct.unpack("<Q", header)[0]
    if not 0 < length <= limit:
        raise ValueError("frame_limit")
    value = handle.read(length)
    if len(value) != length:
        raise ValueError("frame_incomplete")
    return value


def main():
    try:
        source = read_frame(sys.stdin.buffer, MAX_SOURCE)
        reference = read_frame(sys.stdin.buffer, MAX_ARTIFACT)
        answer = read_frame(sys.stdin.buffer, MAX_ARTIFACT)
        if sys.stdin.buffer.read(1):
            raise ValueError("frame_extra")
        sys.stdout.buffer.write(format_buffers(source, reference, answer))
        sys.stdout.buffer.flush()
    except Exception:
        sys.stderr.write("judge_formatter_input_or_format_failure\n")
        raise SystemExit(75) from None


if __name__ == "__main__":
    main()
