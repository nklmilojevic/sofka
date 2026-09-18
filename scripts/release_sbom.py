"""Create an SPDX inventory from the Cargo records for one release binary."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
from urllib.parse import quote

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
SCOPE = (
    "Rust build-input inventory from Cargo compiler-artifact records, including "
    "cached artifacts, build scripts, procedural macros, and their dependencies. "
    "Package features are the union reported during this build. Relationships "
    "identify inputs to the Sofka build, not a dependency graph or linked code. "
    "Native code, system libraries, and the Rust standard library are not inventoried."
)


def sha256(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate_document(document):
    """Validate the SPDX shape emitted by this generator before asset publication."""
    try:
        if (document["spdxVersion"] != "SPDX-2.3"
                or document["dataLicense"] != "CC0-1.0"
                or document["SPDXID"] != "SPDXRef-DOCUMENT"
                or not isinstance(document["name"], str) or not document["name"]
                or not isinstance(document["comment"], str) or not document["comment"]):
            raise ValueError("Invalid SPDX document identity")
        created = document["creationInfo"]
        datetime.strptime(created["created"], "%Y-%m-%dT%H:%M:%SZ")
        if not created["creators"] or not all(isinstance(item, str) and item.startswith("Tool: ") for item in created["creators"]):
            raise ValueError("Invalid SPDX creation information")
        files = document["files"]
        if (len(files) != 1 or files[0]["SPDXID"] != "SPDXRef-Binary"
                or files[0]["fileName"] not in ("./sofka", "./sofka.exe")
                or files[0]["licenseConcluded"] != "NOASSERTION"
                or files[0]["copyrightText"] != "NOASSERTION"):
            raise ValueError("Invalid SPDX binary record")
        checksums = files[0]["checksums"]
        if (len(checksums) != 1 or checksums[0]["algorithm"] != "SHA256"
                or not re.fullmatch(r"[0-9a-f]{64}", checksums[0]["checksumValue"])):
            raise ValueError("Invalid SPDX binary checksum")
        digest = checksums[0]["checksumValue"]
        if not re.fullmatch(r"https://sofka\.rs/sbom/[^/]+/[^/]+/[0-9a-f]{40,64}/" + digest, document["documentNamespace"]):
            raise ValueError("Invalid SPDX namespace or binary association")
        packages = document["packages"]
        ids = {package["SPDXID"] for package in packages}
        if (len(packages) < 2 or len(ids) != len(packages)
                or not all(re.fullmatch(r"SPDXRef-Package-[0-9a-f]{64}", item) for item in ids)):
            raise ValueError("Missing or duplicate SPDX build-input packages")
        root_ids = document["documentDescribes"]
        if len(root_ids) != 1 or root_ids[0] not in ids:
            raise ValueError("Missing SPDX application package")
        root_id = root_ids[0]
        root = next(package for package in packages if package["SPDXID"] == root_id)
        if root["name"] != "sofka" or root["primaryPackagePurpose"] != "APPLICATION":
            raise ValueError("Invalid SPDX application package")
        for package in packages:
            for field in ("name", "versionInfo", "downloadLocation", "licenseConcluded", "licenseDeclared", "copyrightText", "comment"):
                if not isinstance(package[field], str) or not package[field]:
                    raise ValueError("Missing SPDX package field: " + field)
            if package["filesAnalyzed"] is not False:
                raise ValueError("Unexpected SPDX file analysis claim")
        expected = {("SPDXRef-DOCUMENT", "DESCRIBES", root_id), (root_id, "CONTAINS", "SPDXRef-Binary")}
        expected.update((package_id, "BUILD_DEPENDENCY_OF", root_id) for package_id in ids - {root_id})
        edges = document["relationships"]
        actual = {(edge["spdxElementId"], edge["relationshipType"], edge["relatedSpdxElement"]) for edge in edges}
        if len(actual) != len(edges) or actual != expected:
            raise ValueError("SPDX relationships do not cover the build-input packages")
    except (KeyError, TypeError, IndexError) as error:
        raise ValueError("Invalid release SPDX document") from error


def generate(messages, metadata, binary, target, version, revision):
    """Require a successful single-binary release build and bind its inputs to bytes."""
    if not re.fullmatch(r"[a-zA-Z0-9_.-]+", target):
        raise ValueError("Invalid release target")
    if not re.fullmatch(r"[0-9a-f]{40,64}", revision):
        raise ValueError("The source revision must be a full Git commit ID")
    root_id = metadata.get("resolve", {}).get("root")
    packages = {package["id"]: package for package in metadata["packages"]}
    if len(packages) != len(metadata["packages"]):
        raise ValueError("Duplicate Cargo metadata package IDs")
    root = packages.get(root_id)
    if not root or root["name"] != "sofka" or root["version"] != version:
        raise ValueError("Cargo root package does not match the release")
    inputs = {}
    executable = None
    finished = False
    with Path(messages).open(encoding="utf-8") as stream:
        for line in stream:
            message = json.loads(line)
            if finished:
                raise ValueError("Cargo records follow the build completion record")
            reason = message.get("reason")
            if reason == "build-finished":
                if message.get("success") is not True:
                    raise ValueError("Cargo build did not succeed")
                finished = True
            elif reason == "compiler-artifact":
                package_id = message["package_id"]
                if package_id not in packages:
                    raise ValueError("Cargo artifact has no matching metadata package ID")
                kind = set(message["target"]["kind"])
                if message["profile"].get("test") or kind & {"test", "bench", "example"}:
                    raise ValueError("Test, example, or benchmark artifact in release build")
                if "bin" in kind:
                    if (executable is not None or package_id != root_id
                            or message["target"]["name"] != "sofka"
                            or not message.get("executable")
                            or message["profile"].get("debug_assertions") is not False):
                        raise ValueError("Expected one Sofka release executable")
                    executable = Path(message["executable"])
                entry = inputs.setdefault(package_id, {"features": set(), "kinds": set()})
                entry["features"].update(message["features"])
                entry["kinds"].update(kind)
    if not finished or executable is None:
        raise ValueError("Missing successful build completion or Sofka executable")
    expected_name = "sofka.exe" if "windows" in target else "sofka"
    if (executable.name != expected_name or executable.parent.name != "release"
            or executable.parent.parent.name != target):
        raise ValueError("Cargo executable path does not match the release target/profile")
    binary = Path(binary)
    if binary.name != expected_name or not binary.is_file() or binary.is_symlink():
        raise ValueError("The final binary must be a regular Sofka executable")
    digest = sha256(binary)
    if not executable.is_file() or executable.is_symlink() or sha256(executable) != digest:
        raise ValueError("The final binary differs from the recorded Cargo executable")
    ids = {package_id: "SPDXRef-Package-" + hashlib.sha256(package_id.encode()).hexdigest()
           for package_id in inputs}
    records = []
    relationships = [
        {"spdxElementId": "SPDXRef-DOCUMENT", "relationshipType": "DESCRIBES", "relatedSpdxElement": ids[root_id]},
        {"spdxElementId": ids[root_id], "relationshipType": "CONTAINS", "relatedSpdxElement": "SPDXRef-Binary"},
    ]
    for package_id in sorted(inputs):
        package = packages[package_id]
        registry = package.get("source") == CRATES_IO
        name, package_version = package["name"], package["version"]
        record = {
            "SPDXID": ids[package_id], "name": name, "versionInfo": package_version,
            "downloadLocation": (
                f"https://crates.io/api/v1/crates/{quote(name, safe='')}/{quote(package_version, safe='')}/download"
                if registry else "NOASSERTION"
            ),
            "filesAnalyzed": False, "licenseConcluded": "NOASSERTION",
            "licenseDeclared": package.get("license") or "NOASSERTION",
            "copyrightText": "NOASSERTION",
            "comment": "Cargo artifact kinds: " + ", ".join(sorted(inputs[package_id]["kinds"]))
                       + "; features: " + (", ".join(sorted(inputs[package_id]["features"])) or "none") + ".",
        }
        if registry:
            record["externalRefs"] = [{
                "referenceCategory": "PACKAGE-MANAGER", "referenceType": "purl",
                "referenceLocator": f"pkg:cargo/{quote(name, safe='')}@{quote(package_version, safe='')}",
            }]
        if package_id == root_id:
            record["primaryPackagePurpose"] = "APPLICATION"
            record["downloadLocation"] = f"https://github.com/nklmilojevic/sofka/archive/{revision}.tar.gz"
        else:
            relationships.append({"spdxElementId": ids[package_id], "relationshipType": "BUILD_DEPENDENCY_OF", "relatedSpdxElement": ids[root_id]})
        records.append(record)
    document = {
        "spdxVersion": "SPDX-2.3", "dataLicense": "CC0-1.0", "SPDXID": "SPDXRef-DOCUMENT",
        "name": f"sofka-v{version}-{target}",
        "documentNamespace": f"https://sofka.rs/sbom/{version}/{target}/{revision}/{digest}",
        "creationInfo": {"created": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"), "creators": ["Tool: sofka-release-sbom-1"]},
        "comment": f"{SCOPE} Target: {target}. Profile: release. Source revision: {revision}.",
        "documentDescribes": [ids[root_id]], "packages": records,
        "files": [{"SPDXID": "SPDXRef-Binary", "fileName": "./" + expected_name,
                   "checksums": [{"algorithm": "SHA256", "checksumValue": digest}],
                   "licenseConcluded": "NOASSERTION", "copyrightText": "NOASSERTION"}],
        "relationships": relationships,
    }

    validate_document(document)
    return document


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("messages", "metadata", "binary", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    for name in ("target", "version", "revision"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    document = generate(args.messages, json.loads(args.metadata.read_text()), args.binary,
                        args.target, args.version, args.revision)
    args.output.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
