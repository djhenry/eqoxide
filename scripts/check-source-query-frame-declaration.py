#!/usr/bin/env python3
"""Exercise current authored producer artifacts through the paired CPU readers."""
import argparse
import copy
import json
import os
from pathlib import Path, PurePosixPath
import re
import struct
import subprocess
import sys
import tempfile

CAP = "eqoxide-source-query-frames-preserved-v1"
OLD_CAP = "static-collision-query-unit-frames-v1"
FLAG = "--declare-preserved-source-query-frames"
TRIANGLES = "static-collision-triangles-v1"
DECLARED = "declared-source-query-frames-preserved"
MAX_FILE = 8 << 20
MAX_JSON = 1 << 20
FAMILIES = ("static_collision", "eqg_collision", "static_scene_descriptor", "wld_collision")


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def read_file(path, limit=MAX_FILE):
    require(path.is_file(), f"expected regular file: {path}")
    require(path.stat().st_size <= limit, f"file exceeds {limit} bytes: {path}")
    with path.open("rb") as stream:
        data = stream.read(limit + 1)
    require(len(data) <= limit, f"file grew beyond limit: {path}")
    return data


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


def decode_json(data, label):
    require(len(data) <= MAX_JSON, f"JSON byte limit exceeded: {label}")
    try:
        value = json.loads(data, object_pairs_hook=unique_object,
                           parse_constant=lambda token: require(False, f"nonfinite JSON: {token}"))
    except (ValueError, UnicodeError) as error:
        raise RuntimeError(f"invalid JSON from {label}: {error}") from error
    require(isinstance(value, dict), f"expected JSON object: {label}")
    return value


def json_file(path):
    return decode_json(read_file(path, MAX_JSON), str(path))


def git_state(checkout):
    def git(*args):
        result = subprocess.run(["git", "-C", str(checkout), *args], check=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)
        require(len(result.stdout) <= MAX_JSON, "Git state output exceeds limit")
        return result.stdout.decode("utf-8", errors="replace").rstrip("\n")
    commit = git("rev-parse", "HEAD")
    require(re.fullmatch(r"[0-9a-f]{40,64}", commit) is not None, "invalid Git commit")
    status = git("status", "--porcelain=v1", "--untracked-files=all")
    return {"commit": commit, "dirty": bool(status), "dirty_scope": status.splitlines()}


class Commands:
    def __init__(self, scratch):
        self.scratch = scratch
        self.count = 0

    def run(self, checkout, package, example, args, expected_error=None, report=True):
        self.count += 1
        command = ["cargo", "run", "--quiet", "--locked", "--offline",
                   "--manifest-path", str(checkout / "Cargo.toml")]
        if package:
            command += ["-p", package]
        command += ["--example", example, "--", *map(str, args)]
        environment = os.environ.copy()
        variable = ("SOURCE_QUERY_FRAME_CLIENT_TARGET_DIR" if package else
                    "SOURCE_QUERY_FRAME_PRODUCER_TARGET_DIR")
        if environment.get(variable):
            environment["CARGO_TARGET_DIR"] = environment[variable]
        # Compilation diagnostics stay on stderr; stdout is bounded before JSON parsing.
        with tempfile.TemporaryFile(dir=self.scratch) as output, tempfile.TemporaryFile(dir=self.scratch) as errors:
            try:
                result = subprocess.run(command, cwd=checkout, env=environment,
                                        stdout=output, stderr=errors, timeout=120)
            except subprocess.TimeoutExpired as error:
                raise RuntimeError(f"timed out running {example}") from error
            require(output.tell() <= MAX_JSON, f"stdout exceeds limit: {example}")
            require(errors.tell() <= MAX_FILE, f"stderr exceeds limit: {example}")
            output.seek(0)
            stdout = output.read(MAX_JSON + 1)
            errors.seek(0)
            stderr = errors.read(MAX_FILE + 1).decode("utf-8", errors="replace")
        if expected_error is not None:
            require(result.returncode == 1, f"expected semantic exit1 from {example}, got {result.returncode}")
            require(expected_error in stderr, f"{example} refused for wrong reason:\n{stderr}")
            return None
        if stderr:
            sys.stderr.write(stderr)
        require(result.returncode == 0, f"{example} exited {result.returncode}")
        return decode_json(stdout, example) if report else None


def requirements(value, expected, label):
    require(value.get("reader_version") == 2, f"reader version mismatch: {label}")
    caps = value.get("capabilities")
    require(isinstance(caps, list) and len(caps) == len(expected)
            and all(isinstance(cap, str) for cap in caps)
            and set(caps) == set(expected), f"capability set mismatch: {label}: {caps}")


def glb(path):
    data = read_file(path)
    require(len(data) >= 20, f"short GLB: {path}")
    magic, version, total, length, kind = struct.unpack_from("<5I", data)
    require(magic == 0x46546C67 and version == 2 and total == len(data), f"GLB header: {path}")
    require(kind == 0x4E4F534A and length % 4 == 0 and 20 + length <= total, f"JSON chunk: {path}")
    document = decode_json(data[20:20 + length], str(path))
    tail = data[20 + length:]
    if tail:
        require(len(tail) >= 8, f"short BIN chunk: {path}")
        size, kind = struct.unpack_from("<2I", tail)
        require(kind == 0x004E4942 and size % 4 == 0 and size + 8 == len(tail), f"BIN chunk: {path}")
    return document, tail


def compare_modes(default, declared):
    a, abin = glb(default)
    b, bbin = glb(declared)
    ah = a["extras"]["eqoxideAsset"]
    bh = b["extras"]["eqoxideAsset"]
    require(ah["bakeRevision"] != bh["bakeRevision"], f"mode did not affect bake identity: {declared}")
    for header in (ah, bh):
        del header["bakeRevision"]
        del header["requirements"]
    require(a == b and abin == bbin, f"geometry or other GLB semantics changed across modes: {declared}")


def collision_probe(commands, client, path, declared):
    document, _ = glb(path)
    header = document["extras"]["eqoxideAsset"]
    expected = [CAP, TRIANGLES] if declared else [TRIANGLES]
    requirements(header["requirements"], expected, str(path))
    result = commands.run(client, "eqoxide-assets", "static_collision_probe", [path])
    requirements(result["requirements"], expected, f"role probe {path}")
    require(result["query_unit_frames"] == (DECLARED if declared else "unspecified"), f"role frame mismatch: {path}")
    require(result["segment_queries_implemented"] is False, f"role query claim: {path}")
    require(result["bake_revision"] == header["bakeRevision"], f"role revision mismatch: {path}")
    availability = document["extras"]["eqoxideCollision"]["availability"]
    require(result["availability"] == {"available": "Available", "available-empty": "AvailableEmpty"}[availability],
            f"role availability mismatch: {path}")
    return result


def retired_cap_refusal(commands, client, path, destination):
    document, tail = glb(path)
    document = copy.deepcopy(document)
    caps = document["extras"]["eqoxideAsset"]["requirements"]["capabilities"]
    require(caps.count(CAP) == 1, f"negative needs valid declared input: {path}")
    caps[caps.index(CAP)] = OLD_CAP
    encoded = json.dumps(document, separators=(",", ":"), allow_nan=False).encode("utf-8")
    encoded += b" " * (-len(encoded) % 4)
    require(len(encoded) <= MAX_JSON, "rebuilt negative JSON exceeds limit")
    data = struct.pack("<5I", 0x46546C67, 2, 20 + len(encoded) + len(tail),
                       len(encoded), 0x4E4F534A) + encoded + tail
    require(len(data) <= MAX_FILE, "rebuilt negative GLB exceeds limit")
    destination.write_bytes(data)
    commands.run(client, "eqoxide-assets", "static_collision_probe", [destination],
                 expected_error="unsupported collision requirements")


def package_probe(commands, client, root, declared, role_result, refusal=False):
    descriptor = json_file(root / "scene.json")
    package_set = PurePosixPath(descriptor["set"])
    require(not package_set.is_absolute() and ".." not in package_set.parts
            and str(package_set) == descriptor["set"], "unsafe package set")
    paths = [root / "manifest.json"] if (root / "manifest.json").exists() else list(
        (root / "manifests").joinpath(*package_set.parts).glob("*.json"))
    require(len(paths) == 1, f"expected one generated manifest: {root}")
    manifest = json_file(paths[0])
    revision = manifest["revision"]
    require(isinstance(revision, str) and re.fullmatch(r"[0-9a-f]{64}", revision), "invalid manifest revision")
    require(manifest["set"] == descriptor["set"], "manifest/descriptor set mismatch")
    collision = descriptor["components"]["collision"]
    requirements(collision["requirements"], [CAP, TRIANGLES] if declared else [TRIANGLES], "descriptor collision")
    expected = {TRIANGLES, "static-scene-package-v1"}
    if declared:
        expected.add(CAP)
    visual = descriptor["components"]["visual"]
    if visual["availability"] == "available":
        expected.update(visual["requirements"]["capabilities"])
    requirements(descriptor["requirements"], expected, "descriptor root")
    requirements(manifest["requirements"], expected, "manifest root")
    references = {"scene.json"}
    for component in descriptor["components"].values():
        if component["availability"] in ("available", "available-empty"):
            references.add(component["file"]["path"])
    entries = manifest["files"]
    require(isinstance(entries, list) and 1 <= len(entries) <= 3, "manifest file count")
    require(len({entry["path"] for entry in entries}) == len(entries)
            and {entry["path"] for entry in entries} == references, "manifest/descriptor file set mismatch")
    arguments = [paths[0], descriptor["zone"], revision]
    for entry in entries:
        logical = entry["path"]
        relative = PurePosixPath(logical)
        require(not relative.is_absolute() and ".." not in relative.parts and str(relative) == logical,
                "unsafe logical package path")
        disk = root.joinpath(*relative.parts)
        data = read_file(disk)
        require(entry["size"] == len(data), f"supplied-file size mismatch: {logical}")
        arguments += ["--file", logical, disk]
    result = commands.run(client, "eqoxide", "static_scene_package_probe", arguments,
                          expected_error="collision policy mismatch" if refusal else None)
    if refusal:
        return
    require(result["manifest_revision"] == revision and result["set"] == descriptor["set"]
            and result["zone"] == descriptor["zone"] and result["bake_revision"] == descriptor["bakeRevision"],
            "package identity mismatch")
    require(result["assembled_file_content_verified"] is True, "assembled content was not verified")
    require(result["runtime_activation"] is False and result["segment_queries"] is False, "package runtime/query claim")
    require(result["individual_cas_chunks_verified"] is False, "unexpected CAS-chunk verification claim")
    require(result["availability"]["collision"] == collision["availability"], "package collision availability")
    report = result["collision"]
    require(report["query_unit_frames"] == role_result["query_unit_frames"], "package/role frame mismatch")
    require(report["meshes"] == role_result["meshes"] and report["instances"] == role_result["instances"],
            "package/role geometry count mismatch")
    require(report["bounds"] == role_result["bounds_server_geometry"], "package/role bounds mismatch")
    policy = {"FrontFaces": "inspection-segment-front-v1", "BothFaces": "inspection-segment-both-v1"}
    require(report["declared_policy"] == policy[role_result["declared_query_policy"]]
            == collision["queryPolicy"], "package/role policy mismatch")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("client_checkout", type=Path, metavar="CLIENT_CHECKOUT")
    parser.add_argument("producer_checkout", type=Path, metavar="PRODUCER_CHECKOUT")
    args = parser.parse_args()
    client, producer = args.client_checkout.resolve(), args.producer_checkout.resolve()
    for checkout in (client, producer):
        require((checkout / "Cargo.toml").is_file() and (checkout / "Cargo.lock").is_file(), "complete Cargo checkout required")
    before = {"client": git_state(client), "producer": git_state(producer)}
    print(json.dumps({"checkouts": before}, sort_keys=True), flush=True)
    with tempfile.TemporaryDirectory(prefix="eqoxide-source-query-frames-") as temporary:
        scratch = Path(temporary)
        commands = Commands(scratch)
        for family in FAMILIES:
            example = family + "_fixture"
            refused = scratch / (family + "-old-flag")
            commands.run(producer, None, example, [refused, "--canonical-query-units"],
                         expected_error="unknown fixture option", report=False)
            require(not refused.exists(), f"retired flag created output: {family}")
            outputs = {}
            for declared in (False, True):
                root = scratch / family / ("preserved" if declared else "unspecified")
                commands.run(producer, None, example, [root, FLAG] if declared else [root], report=False)
                outputs[declared] = root
                relative_paths = ({"static_collision": ["collision.glb", "empty.glb"],
                                   "eqg_collision": ["collision.glb", "empty.glb"],
                                   "static_scene_descriptor": ["collision.glb"],
                                   "wld_collision": ["mixed/collision.glb", "empty/collision.glb", "disagreement/collision.glb"]})[family]
                for index, relative in enumerate(relative_paths):
                    path = root / relative
                    role = collision_probe(commands, client, path, declared)
                    if declared:
                        retired_cap_refusal(commands, client, path, scratch / f"old-cap-{family}-{index}.glb")
                    if family in ("static_scene_descriptor", "wld_collision"):
                        package_probe(commands, client, path.parent, declared, role, "disagreement/" in relative)
            for relative in relative_paths:
                compare_modes(outputs[False] / relative, outputs[True] / relative)
            if family == "static_scene_descriptor":
                compare_modes(outputs[False] / "visual.glb", outputs[True] / "visual.glb")
            print(f"{family}: default/declared role, geometry and refusal checks passed", flush=True)
        after = {"client": git_state(client), "producer": git_state(producer)}
        require(before == after, "checkout commit/dirty scope changed during conformance; rerun against stable sources")
        print(json.dumps({"passed": True, "cargo_invocations": commands.count,
                          "checkouts": after, "segment_queries": False,
                          "runtime_activation": False, "native_agreement_verified": False}, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, KeyError, TypeError, ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"source query-frame conformance failed: {error}", file=sys.stderr)
        sys.exit(1)
