#!/usr/bin/env python3
"""Collect actual license/notice files from the locked native dependency graph."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tomllib
from urllib.parse import urlsplit


LICENSE_NAME = re.compile(r"^(?:licen[sc]e|copying|copyright|notice|unlicense)(?:$|[._-])", re.I)
LICENSE_DIRS = {"license", "licenses", "licence", "licences"}


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def source_boundary(package: dict, repo: Path) -> Path:
    directory = Path(package["manifest_path"]).parent.resolve()
    source = package.get("source")
    if source is None and directory.is_relative_to(repo):
        return repo
    if source and source.startswith("git+"):
        for candidate in [directory, *directory.parents]:
            if (candidate / ".git").exists():
                return candidate
    return directory


def license_files(package: dict, repo: Path) -> list[tuple[Path, str]]:
    directory = Path(package["manifest_path"]).parent.resolve()
    boundary = source_boundary(package, repo)
    found: dict[Path, str] = {}
    scopes = [(directory, "package")]
    if boundary != directory:
        scopes.append((boundary, "repository"))
    for root, label in scopes:
        for item in root.iterdir():
            if item.is_symlink():
                continue
            if item.is_file() and LICENSE_NAME.match(item.name):
                found[item] = f"{label}/{item.name}"
            elif item.is_dir() and item.name.lower() in LICENSE_DIRS:
                for nested in item.rglob("*"):
                    if nested.is_file() and not nested.is_symlink():
                        found[nested] = f"{label}/{nested.relative_to(root).as_posix()}"
    declared = package.get("license_file")
    if declared:
        item = (directory / declared).resolve()
        if item.is_relative_to(boundary) and item.is_file() and not item.is_symlink():
            label = "package" if item.is_relative_to(directory) else "repository"
            base = directory if label == "package" else boundary
            found[item] = f"{label}/{item.relative_to(base).as_posix()}"
    return sorted(found.items(), key=lambda entry: entry[1])


def public_source(value: str | None) -> str:
    if value is None:
        return "workspace"
    # Cargo source URLs should be public. Refuse to disclose credentials or a
    # local source path if a future dependency uses either representation.
    raw = value.removeprefix("git+").removeprefix("registry+").removeprefix("sparse+")
    parsed = urlsplit(raw)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname or parsed.username or parsed.password:
        raise ValueError("dependency source is not a credential-free public HTTP(S) URL")
    return value


def supplement_files(package: dict, repo: Path) -> tuple[list[tuple[Path, str]], dict | None]:
    folder = repo / "licenses" / "rust-supplements" / f"{package['name']}-{package['version']}"
    provenance_path = folder / "provenance.json"
    if not provenance_path.is_file():
        return [], None
    provenance = json.loads(provenance_path.read_text(encoding="utf-8"))
    package_root = Path(package["manifest_path"]).parent
    if (provenance["package"], provenance["version"]) != (package["name"], package["version"]):
        raise ValueError(f"license supplement does not bind the installed {package['name']} source")
    vcs_path = package_root / ".cargo_vcs_info.json"
    if vcs_path.is_file():
        vcs = json.loads(vcs_path.read_text(encoding="utf-8"))
        if provenance["package_vcs_revision"] != vcs["git"]["sha1"]:
            raise ValueError("license supplement VCS revision does not match")
    else:
        # Some historical archives omit VCS metadata. Bind those to the locked
        # archive and compare every packaged source file with a pinned upstream
        # tree. Generated release payloads have separate SHA256 bindings.
        if provenance.get("package_vcs_revision") is not None:
            raise ValueError("license supplement invents unavailable VCS metadata")
        comparison = provenance["source_comparison"]
        if (comparison["repository"] != package["repository"]
                or comparison["revision"] != provenance["license_revision"]
                or not re.fullmatch(r"[0-9a-f]{40}", comparison["revision"])):
            raise ValueError("license supplement repository comparison does not match")
        public_source(comparison["repository"])
        locked = tomllib.loads((repo / "Cargo.lock").read_text(encoding="utf-8"))["package"]
        checksums = [entry.get("checksum") for entry in locked
                     if (entry["name"], entry["version"], entry.get("source"))
                     == (package["name"], package["version"], package.get("source"))]
        if checksums != [provenance["package_archive_sha256"]]:
            raise ValueError("license supplement archive checksum does not match Cargo.lock")
        tracked = comparison["tracked_files"]
        generated = comparison.get("generated_files", {})
        if not {"Cargo.toml.orig", "src/lib.rs"} <= tracked.keys() or tracked.keys() & generated.keys():
            raise ValueError("license supplement lacks its manifest/source comparison")
        actual = {item.relative_to(package_root).as_posix() for item in package_root.rglob("*")
                  if item.is_file() and item.name not in {
                      ".cargo-ok", ".cargo-checksum.json", "Cargo.toml"}}
        if actual != tracked.keys() | generated.keys():
            raise ValueError("license supplement comparison omits packaged files")
        for relative, expected in tracked.items():
            item = package_root / relative
            if item.is_symlink() or item.resolve() != item or not item.is_relative_to(package_root):
                raise ValueError("license supplement source path is invalid")
            data = item.read_bytes()
            if hashlib.sha1(f"blob {len(data)}\0".encode() + data).hexdigest() != expected["git_blob_sha1"]:
                raise ValueError("license supplement tracked source no longer matches")
        for relative, expected in generated.items():
            item = package_root / relative
            if item.is_symlink() or item.resolve() != item or not item.is_relative_to(package_root):
                raise ValueError("license supplement generated path is invalid")
            if digest(item) != expected:
                raise ValueError("license supplement generated payload no longer matches")
    if (provenance["package_vcs_revision"] is not None
            and provenance["license_revision"] != provenance["package_vcs_revision"]):
        comparison = provenance["comparison"]
        if comparison["commits"] != 1 or comparison["changed_files"] != ["LICENSE"]:
            raise ValueError("unsupported post-release license source change")
        for relative, expected in comparison["unchanged_source"].items():
            data = (package_root / relative).read_bytes()
            actual = hashlib.sha1(f"blob {len(data)}\0".encode() + data).hexdigest()
            if actual != expected:
                raise ValueError("post-release license supplement source no longer matches")
    files = []
    for entry in provenance["files"]:
        relative = Path(entry["file"])
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError("license supplement has an invalid file path")
        original = folder / relative
        if original.is_symlink() or digest(original) != entry["sha256"]:
            raise ValueError("license supplement content digest does not match")
        files.append((original, f"supplement/{relative.as_posix()}"))
    return files, provenance


def collect(repo: Path, target: str, destination: Path, require_complete: bool = False,
            *, all_dependencies: bool = False) -> dict:
    repo = repo.resolve()
    command = ["cargo", "metadata", "--locked", "--format-version", "1"]
    if all_dependencies:
        command.append("--offline")
    else:
        command.extend(["--filter-platform", target])
    completed = subprocess.run(command, cwd=repo, check=True, capture_output=True, text=True)
    metadata = json.loads(completed.stdout)
    packages = {entry["id"]: entry for entry in metadata["packages"]}
    nodes = {entry["id"]: entry for entry in metadata["resolve"]["nodes"]}
    roots = [key for key, entry in packages.items() if entry["name"] == "gptgrep-cli" and key in metadata["workspace_members"]]
    if len(roots) != 1:
        raise ValueError("expected one workspace gptgrep-cli package")
    queue, selected = list(packages) if all_dependencies else list(roots), set()
    while queue:
        current = queue.pop()
        if current in selected:
            continue
        selected.add(current)
        for dependency in nodes[current]["deps"]:
            if any(kind["kind"] in (None, "build") for kind in dependency["dep_kinds"]):
                queue.append(dependency["pkg"])
    destination.mkdir(parents=True, exist_ok=False)
    records, missing = [], []
    for key in sorted(selected, key=lambda item: (packages[item]["name"], packages[item]["version"], item)):
        package = packages[key]
        source = public_source(package.get("source"))
        source_id = hashlib.sha256(source.encode()).hexdigest()[:10]
        folder = f"{package['name']}-{package['version']}-{source_id}"
        payload = []
        files = license_files(package, repo)
        supplement = None
        if not files:
            files, supplement = supplement_files(package, repo)
        for original, relative in files:
            copied = destination / folder / relative
            copied.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(original, copied)
            copied.chmod(0o644)
            payload.append({"file": f"{folder}/{relative}", "sha256": digest(copied)})
        record = {
            "name": package["name"], "version": package["version"], "source": source,
            "declared_license": package.get("license"), "files": payload,
            "license_files_available": bool(payload),
        }
        if supplement:
            record["supplement"] = supplement
        records.append(record)
        if not payload:
            missing.append({"name": package["name"], "version": package["version"], "declared_license": package.get("license")})
    inventory = {
        "schema_version": "gptgrep.licenses.v1", "target": target,
        "scope": ("all resolved workspace dependencies, including dev and conditional targets"
                  if all_dependencies else
                  "normal and build dependencies reachable from gptgrep-cli; dev edges excluded"),
        "complete": not missing, "packages": records, "missing_license_files": missing,
        "note": "SPDX declarations are retained as metadata; only listed source files were copied. This inventory does not replace the files or a license compatibility review.",
    }
    (destination / "inventory.json").write_text(json.dumps(inventory, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if require_complete and missing:
        names = ", ".join(f"{item['name']} {item['version']}" for item in missing)
        raise ValueError(f"actual license files unavailable for {names}; inventory.json records the gap")
    return inventory


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--require-complete", action="store_true")
    parser.add_argument("--all-dependencies", action="store_true",
                        help="Collect every resolved source for a vendored source SDK")
    args = parser.parse_args()
    inventory = collect(args.repo_root, args.target, args.output, args.require_complete,
                        all_dependencies=args.all_dependencies)
    print(json.dumps({"complete": inventory["complete"], "packages": len(inventory["packages"]), "missing_license_files": inventory["missing_license_files"]}))


if __name__ == "__main__":
    main()
