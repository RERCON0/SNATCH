#!/usr/bin/env python3
"""Build and verify SNATCH Windows packages; Python 3.13 + OpenSSL 3, no pip."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PUBLIC_KEY = ROOT / "release/public-key.pem"
TARGET = "x86_64-pc-windows-msvc"
PAYLOAD = {"snatch.exe", "snatch-app.exe", "README.md", "LICENSE", "FONT-LICENSE.txt"}
META = {"manifest.json", "manifest.sig", "public-key.pem"}
MAX_FILE = 64 * 1024 * 1024
MAX_PACKAGE = 160 * 1024 * 1024
MAX_META = 2 * 1024 * 1024
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")


def run(args: list[str], *, cwd: Path = ROOT, capture: bool = True, timeout: int = 120) -> bytes:
    result = subprocess.run(args, cwd=cwd, check=True, timeout=timeout,
                            stdout=subprocess.PIPE if capture else None,
                            stderr=subprocess.PIPE if capture else None)
    return result.stdout if capture else b""


def openssl() -> str:
    found = shutil.which("openssl")
    if not found and os.name == "nt":
        for rel in ("Git/mingw64/bin/openssl.exe", "Git/usr/bin/openssl.exe"):
            candidate = Path(os.environ.get("ProgramFiles", "C:/Program Files")) / rel
            if candidate.is_file():
                found = str(candidate)
                break
    if not found:
        raise ValueError("OpenSSL 3 is required (Windows: included with Git for Windows)")
    if not run([found, "version"]).startswith(b"OpenSSL 3."):
        raise ValueError("OpenSSL 3 is required")
    return found


def canonical(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(",", ":")) + "\n").encode()


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def public_der(path: Path) -> bytes:
    value = run([openssl(), "pkey", "-pubin", "-in", str(path), "-outform", "DER"])
    if len(value) != 44 or not value.startswith(ED25519_SPKI_PREFIX):
        raise ValueError("Only Ed25519 release keys are accepted")
    return value


def sign(manifest: bytes, key: Path) -> bytes:
    with tempfile.TemporaryDirectory(prefix="snatch-sign-") as folder:
        data, signature = Path(folder) / "manifest", Path(folder) / "signature"
        data.write_bytes(manifest)
        run([openssl(), "pkeyutl", "-sign", "-rawin", "-inkey", str(key),
             "-in", str(data), "-out", str(signature)])
        return signature.read_bytes()


def verify_signature(manifest: bytes, signature: bytes, key: Path) -> None:
    if len(signature) != 64:
        raise ValueError("Invalid Ed25519 signature length")
    with tempfile.TemporaryDirectory(prefix="snatch-verify-") as folder:
        data, sig = Path(folder) / "manifest", Path(folder) / "signature"
        data.write_bytes(manifest)
        sig.write_bytes(signature)
        run([openssl(), "pkeyutl", "-verify", "-rawin", "-pubin", "-inkey", str(key),
             "-in", str(data), "-sigfile", str(sig)])


def pe_info(data: bytes, subsystem: int) -> dict:
    """Reject wrong architecture, missing exploit mitigations and dynamic CRT."""
    def unpack(fmt: str, offset: int) -> tuple:
        size = struct.calcsize(fmt)
        if offset < 0 or offset + size > len(data):
            raise ValueError("Truncated PE image")
        return struct.unpack_from(fmt, data, offset)

    if data[:2] != b"MZ":
        raise ValueError("Missing DOS header")
    pe, = unpack("<I", 0x3C)
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("Missing PE header")
    machine, sections = unpack("<HH", pe + 4)
    opt_size, = unpack("<H", pe + 20)
    opt = pe + 24
    magic, = unpack("<H", opt)
    actual_subsystem, flags = unpack("<HH", opt + 68)
    if machine != 0x8664 or magic != 0x20B or actual_subsystem != subsystem:
        raise ValueError("Expected Windows x64 PE with the correct CLI/GUI subsystem")
    if flags & 0x160 != 0x160:
        raise ValueError("PE requires ASLR, high-entropy ASLR and DEP")
    if opt_size < 128 or sections == 0 or sections > 96:
        raise ValueError("Invalid PE sections/optional header")
    mappings = []
    for number in range(sections):
        offset = opt + opt_size + 40 * number
        _, virtual_size, rva, raw_size, raw = unpack("<8sIIII", offset)
        mappings.append((rva, max(virtual_size, raw_size), raw, raw_size))

    def position(rva: int, size: int = 1) -> int:
        for start, length, raw, raw_size in mappings:
            delta = rva - start
            if 0 <= delta < length and delta + size <= raw_size:
                at = raw + delta
                if at + size <= len(data):
                    return at
        raise ValueError("PE RVA outside file-backed sections")

    imports_rva, imports_size = unpack("<II", opt + 120)
    imports = []
    if not imports_rva or imports_size < 20 or imports_size > MAX_META:
        raise ValueError("Missing/invalid PE imports")
    for index in range(min(imports_size // 20, 4096)):
        descriptor = unpack("<IIIII", position(imports_rva + index * 20, 20))
        if not any(descriptor):
            break
        at = position(descriptor[3])
        name = data[at:at + 256].split(b"\0", 1)[0].decode("ascii").lower()
        if not re.fullmatch(r"[a-z0-9_.-]+\.dll", name):
            raise ValueError("Invalid PE import name")
        if name.startswith(("vcruntime", "msvcp", "api-ms-win-crt")) or name == "ucrtbase.dll":
            raise ValueError(f"Dynamic Visual C++ runtime dependency: {name}")
        imports.append(name)
    else:
        raise ValueError("Unterminated PE import table")
    return {"machine": "amd64", "subsystem": subsystem, "dll_characteristics": flags,
            "imports": sorted(set(imports))}


def reject_duplicates(pairs: list[tuple[str, object]]) -> dict:
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"Duplicate JSON field: {key}")
        value[key] = item
    return value


def verify_package(path: Path, trusted_key: Path = PUBLIC_KEY) -> dict:
    """Do not extract or execute any member; the key must be trusted externally."""
    trusted_der = public_der(trusted_key)
    if path.stat().st_size > MAX_PACKAGE:
        raise ValueError("Package exceeds size limit")
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        names = [entry.filename for entry in entries]
        if len(names) != len(set(names)) or set(names) != PAYLOAD | META:
            raise ValueError("Unexpected, missing or duplicate ZIP members")
        if sum(entry.file_size for entry in entries) > MAX_PACKAGE:
            raise ValueError("Expanded ZIP exceeds size limit")
        for entry in entries:
            limit = MAX_META if entry.filename in META else MAX_FILE
            mode = entry.external_attr >> 16
            if entry.file_size <= 0 or entry.file_size > limit or entry.flag_bits & 1:
                raise ValueError("Invalid ZIP member size/encryption")
            if entry.compress_type not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
                raise ValueError("Unsupported ZIP compression")
            if stat.S_IFMT(mode) not in (0, stat.S_IFREG):
                raise ValueError("Non-regular ZIP member")
        # read() verifies each member's CRC; size limits were enforced above.
        content = {name: archive.read(name) for name in names}
    with tempfile.TemporaryDirectory(prefix="snatch-key-") as folder:
        bundled = Path(folder) / "public.pem"
        bundled.write_bytes(content["public-key.pem"])
        if public_der(bundled) != trusted_der:
            raise ValueError("Bundled key differs from the trusted SNATCH key")
    raw = content["manifest.json"]
    verify_signature(raw, content["manifest.sig"], trusted_key)
    manifest = json.loads(raw, object_pairs_hook=reject_duplicates)
    expected_fields = {"schema", "project", "version", "target", "source", "toolchain", "files", "signer_sha256"}
    if not isinstance(manifest, dict) or set(manifest) != expected_fields or canonical(manifest) != raw:
        raise ValueError("Invalid/non-canonical manifest schema")
    if type(manifest["schema"]) is not int or manifest["schema"] != 1 or manifest["project"] != "SNATCH" or manifest["target"] != TARGET:
        raise ValueError("Invalid manifest project/target")
    if not isinstance(manifest["version"], str) or not re.fullmatch(r"\d+\.\d+\.\d+", manifest["version"]):
        raise ValueError("Invalid version")
    source = manifest["source"]
    if not isinstance(source, dict) or set(source) != {"commit", "tree", "inputs_sha256", "url"}:
        raise ValueError("Invalid source provenance")
    for field in ("commit", "tree"):
        if not isinstance(source[field], str) or not re.fullmatch(r"[0-9a-f]{40}", source[field]):
            raise ValueError("Invalid source revision")
    if not isinstance(source["inputs_sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", source["inputs_sha256"]):
        raise ValueError("Invalid source input hash")
    if source["url"] != "https://github.com/RERCON0/SNATCH/tree/" + source["commit"]:
        raise ValueError("Invalid source URL")
    toolchain = manifest["toolchain"]
    if not isinstance(toolchain, dict) or set(toolchain) != {"rustc", "cargo", "cargo_lock_sha256"}:
        raise ValueError("Invalid toolchain provenance")
    if not all(isinstance(v, str) and 0 < len(v) < 4096 for v in toolchain.values()):
        raise ValueError("Invalid toolchain fields")
    if not re.fullmatch(r"[0-9a-f]{64}", toolchain["cargo_lock_sha256"]):
        raise ValueError("Invalid lockfile hash")
    if manifest["signer_sha256"] != sha256(trusted_der):
        raise ValueError("Invalid signer fingerprint")
    files = manifest["files"]
    if not isinstance(files, dict) or set(files) != PAYLOAD:
        raise ValueError("Invalid payload inventory")
    for name in sorted(PAYLOAD):
        record = files[name]
        fields = {"sha256", "size", "pe"} if name.endswith(".exe") else {"sha256", "size"}
        if not isinstance(record, dict) or set(record) != fields:
            raise ValueError("Invalid payload record")
        if type(record["size"]) is not int or record["size"] != len(content[name]) or record["sha256"] != sha256(content[name]):
            raise ValueError(f"Payload hash/size mismatch: {name}")
        if name.endswith(".exe") and record["pe"] != pe_info(content[name], 3 if name == "snatch.exe" else 2):
            raise ValueError("PE metadata mismatch")
    return manifest


def source_state() -> dict:
    if run(["git", "status", "--porcelain", "--untracked-files=all"]).strip():
        raise ValueError("Release builds require a clean committed checkout")
    commit = run(["git", "rev-parse", "HEAD"]).decode().strip()
    tree = run(["git", "rev-parse", "HEAD^{tree}"]).decode().strip()
    tracked = run(["git", "ls-files", "-z"]).decode().split("\0")
    inputs = {}
    for name in sorted(filter(None, tracked)):
        path = ROOT / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"Source input is not a regular file: {name}")
        inputs[name] = sha256(path.read_bytes())
    return {"commit": commit, "tree": tree, "inputs_sha256": sha256(canonical(inputs)),
            "url": "https://github.com/RERCON0/SNATCH/tree/" + commit}


def package(payload: dict[str, bytes], source: dict, toolchain: dict, version: str,
            private_key: Path, public_key: Path, output: Path) -> dict:
    der = public_der(public_key)
    derived = run([openssl(), "pkey", "-in", str(private_key), "-pubout", "-outform", "DER"])
    if derived != der:
        raise ValueError("Signing key does not match the trusted public key")
    if set(payload) != PAYLOAD:
        raise ValueError("Invalid build payload")
    files = {}
    for name, data in payload.items():
        if not data or len(data) > MAX_FILE:
            raise ValueError(f"Invalid payload size: {name}")
        files[name] = {"sha256": sha256(data), "size": len(data)}
        if name.endswith(".exe"):
            files[name]["pe"] = pe_info(data, 3 if name == "snatch.exe" else 2)
    manifest = {"schema": 1, "project": "SNATCH", "version": version, "target": TARGET,
                "source": source, "toolchain": toolchain, "files": files, "signer_sha256": sha256(der)}
    raw = canonical(manifest)
    entries = {**payload, "manifest.json": raw, "manifest.sig": sign(raw, private_key),
               "public-key.pem": public_key.read_bytes()}
    output.parent.mkdir(parents=True, exist_ok=True)
    # Unique sibling + os.replace: never leave a partial archive at the release path.
    descriptor, temporary = tempfile.mkstemp(prefix=output.name + ".", suffix=".tmp", dir=output.parent)
    os.close(descriptor)
    candidate = Path(temporary)
    try:
        with zipfile.ZipFile(candidate, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for name, data in sorted(entries.items()):
                info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | 0o644) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, data)
        verify_package(candidate, public_key)
        os.replace(candidate, output)
    finally:
        candidate.unlink(missing_ok=True)
    return manifest


def build(output: Path, private_key: Path) -> dict:
    if sys.platform != "win32":
        raise ValueError("Release builds require native Windows MSVC")
    if private_key.resolve().is_relative_to(ROOT):
        raise ValueError("Private signing keys must be stored outside the repository")
    disallowed = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER",
                  "RUSTC_WORKSPACE_WRAPPER", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET"}
    for name in os.environ:
        if name in disallowed or name.startswith("CARGO_PROFILE_RELEASE_"):
            raise ValueError(f"Release build environment override is forbidden: {name}")
    source = source_state()
    channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    rustc = run(["rustc", "--version", "--verbose"]).decode().strip()
    if not rustc.startswith("rustc " + channel + " "):
        raise ValueError("Compiler differs from rust-toolchain.toml")
    toolchain = {"rustc": rustc, "cargo": run(["cargo", "--version"]).decode().strip(),
                 "cargo_lock_sha256": sha256((ROOT / "Cargo.lock").read_bytes())}
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    # Fresh target tree prevents stale/foreign executables from being packaged.
    with tempfile.TemporaryDirectory(prefix="snatch-build-") as folder:
        target = Path(folder) / "target"
        run(["cargo", "build", "--locked", "--release", "--bins", "--target", TARGET,
             "--target-dir", str(target)], capture=False, timeout=2400)
        built = target / TARGET / "release"
        payload = {name: (built / name).read_bytes() for name in ("snatch.exe", "snatch-app.exe")}
        payload.update({"README.md": (ROOT / "README.md").read_bytes(),
                        "LICENSE": (ROOT / "LICENSE").read_bytes(),
                        "FONT-LICENSE.txt": (ROOT / "fonts/OFL-notice.txt").read_bytes()})
        if source_state() != source:
            raise ValueError("Source checkout changed during compilation")
        return package(payload, source, toolchain, version, private_key, PUBLIC_KEY, output)


def keygen(private: Path, public: Path) -> None:
    if private.resolve().is_relative_to(ROOT):
        raise ValueError("Private keys must live outside the repository")
    if private.exists() or public.exists():
        raise ValueError("Refusing to overwrite an existing signing key")
    private.parent.mkdir(parents=True, exist_ok=True)
    if os.name == "nt":
        account = os.environ["USERDOMAIN"] + "\\" + os.environ["USERNAME"]
        run(["icacls", str(private.parent), "/inheritance:r", "/grant:r", account + ":(OI)(CI)F"])
    else:
        private.parent.chmod(0o700)
    run([openssl(), "genpkey", "-algorithm", "ED25519", "-out", str(private)])
    if os.name != "nt":
        private.chmod(0o600)
    public.parent.mkdir(parents=True, exist_ok=True)
    run([openssl(), "pkey", "-in", str(private), "-pubout", "-out", str(public)])
    print("Public key SHA-256:", sha256(public_der(public)))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    verify = commands.add_parser("verify", help="Verify without extracting or running binaries")
    verify.add_argument("archive", type=Path)
    verify.add_argument("--public-key", type=Path, default=PUBLIC_KEY)
    release = commands.add_parser("build", help="Clean source -> fresh MSVC build -> signed, verified ZIP")
    release.add_argument("--private-key", type=Path, required=True)
    release.add_argument("--output", type=Path, default=ROOT / "dist/snatch-windows-x64.zip")
    keys = commands.add_parser("keygen", help="Create an independent publisher key; never overwrites")
    keys.add_argument("--private-key", type=Path, required=True)
    keys.add_argument("--public-key", type=Path, default=PUBLIC_KEY)
    binaries = commands.add_parser("check-binaries", help="Inspect unsigned CI candidates, without running them")
    binaries.add_argument("directory", type=Path)
    args = parser.parse_args()
    if args.command == "check-binaries":
        for name, subsystem in (("snatch.exe", 3), ("snatch-app.exe", 2)):
            path = args.directory / name
            if not 0 < path.stat().st_size <= MAX_FILE:
                raise ValueError("Invalid executable size")
            data = path.read_bytes()
            print(name, sha256(data), json.dumps(pe_info(data, subsystem), sort_keys=True))
        return
    if args.command == "keygen":
        keygen(args.private_key, args.public_key)
        return
    if args.command == "verify":
        manifest = verify_package(args.archive, args.public_key)
    else:
        manifest = build(args.output, args.private_key)
    print(f"Verified SNATCH {manifest['version']} / {manifest['source']['commit']} / {manifest['signer_sha256']}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError, zipfile.BadZipFile, KeyError, TypeError) as error:
        print(f"Release rejected: {error}", file=sys.stderr)
        sys.exit(1)
