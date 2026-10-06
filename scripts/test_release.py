"""Real Ed25519 positive/negative package tests, offline on Windows and Linux."""
import copy
from pathlib import Path
import stat
import struct
import tempfile
import unittest
import warnings
import zipfile

import release


def executable(subsystem=3):
    data = bytearray(1024)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 0x80)
    data[0x80:0x84] = b"PE\0\0"
    struct.pack_into("<HH", data, 0x84, 0x8664, 1)
    struct.pack_into("<H", data, 0x94, 240)
    opt = 0x98
    struct.pack_into("<H", data, opt, 0x20B)
    struct.pack_into("<HH", data, opt + 68, subsystem, 0x160)
    struct.pack_into("<II", data, opt + 120, 0x1000, 40)
    struct.pack_into("<8sIIII", data, opt + 240, b".rdata", 512, 0x1000, 512, 512)
    struct.pack_into("<IIIII", data, 512, 0x1080, 0, 0, 0x1040, 0x1080)
    data[576:589] = b"kernel32.dll\0"
    return bytes(data)


class ReleaseTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="snatch-release-test-")
        cls.folder = Path(cls.temporary.name)
        cls.key, cls.public = cls.folder / "private.pem", cls.folder / "public.pem"
        release.keygen(cls.key, cls.public)
        cls.other_key, cls.other_public = cls.folder / "other.pem", cls.folder / "other-public.pem"
        release.keygen(cls.other_key, cls.other_public)
        cls.payload = {"snatch.exe": executable(), "snatch-app.exe": executable(2),
                       "README.md": b"readme\n", "LICENSE": b"GPL\n", "FONT-LICENSE.txt": b"OFL\n"}
        cls.source = {"commit": "a" * 40, "tree": "b" * 40, "inputs_sha256": "c" * 64,
                      "url": "https://github.com/RERCON0/SNATCH/tree/" + "a" * 40}
        cls.tools = {"rustc": "rustc 1.99.0", "cargo": "cargo 1.99.0", "cargo_lock_sha256": "d" * 64}
        cls.archive = cls.folder / "valid.zip"
        cls.manifest = release.package(cls.payload, cls.source, cls.tools, "0.5.3",
                                       cls.key, cls.public, cls.archive)
        with zipfile.ZipFile(cls.archive) as archive:
            cls.entries = {name: archive.read(name) for name in archive.namelist()}

    @classmethod
    def tearDownClass(cls):
        cls.temporary.cleanup()

    def mutated(self, entries, name="bad.zip", extra=None):
        path = self.folder / name
        with zipfile.ZipFile(path, "w") as archive:
            for entry, data in entries.items():
                archive.writestr(entry, data)
            if extra:
                with warnings.catch_warnings():
                    warnings.simplefilter("ignore", UserWarning)
                    archive.writestr(*extra)
        return path

    def reject(self, path, key=None):
        with self.assertRaises((ValueError, OSError, release.subprocess.SubprocessError, zipfile.BadZipFile)):
            release.verify_package(path, key or self.public)

    def test_valid_signature_and_provenance(self):
        self.assertEqual(release.verify_package(self.archive, self.public), self.manifest)

    def test_tampering_each_payload_is_rejected(self):
        for name in release.PAYLOAD:
            with self.subTest(name=name):
                entries = dict(self.entries)
                entries[name] += b"tampered"
                self.reject(self.mutated(entries))

    def test_signature_and_manifest_tampering(self):
        for name in ("manifest.json", "manifest.sig"):
            entries = dict(self.entries)
            entries[name] = bytes([entries[name][0] ^ 1]) + entries[name][1:]
            self.reject(self.mutated(entries))

    def test_extra_missing_duplicate_and_traversal_members(self):
        for name in ("../escape.exe", "/absolute.exe", "extra.dll", "snatch.exe", "SNATCH.EXE"):
            self.reject(self.mutated(self.entries, extra=(name, b"extra")))
        entries = dict(self.entries)
        del entries["LICENSE"]
        self.reject(self.mutated(entries))

    def test_attacker_signed_package_is_rejected_by_trusted_key(self):
        path = self.folder / "attacker.zip"
        release.package(self.payload, self.source, self.tools, "0.5.3", self.other_key, self.other_public, path)
        self.reject(path)
        self.reject(self.archive, self.other_public)

    def test_signed_invalid_schema_hash_size_and_provenance_are_rejected(self):
        for alteration in ("extra", "hash", "size", "source", "project", "pe", "bool", "schema_bool"):
            with self.subTest(alteration=alteration):
                manifest = copy.deepcopy(self.manifest)
                if alteration == "extra":
                    manifest["unexpected"] = True
                elif alteration == "hash":
                    manifest["files"]["LICENSE"]["sha256"] = "0" * 64
                elif alteration == "size":
                    manifest["files"]["LICENSE"]["size"] += 1
                elif alteration == "source":
                    manifest["source"]["commit"] = "dirty"
                elif alteration == "project":
                    manifest["project"] = "OtherProject"
                elif alteration == "pe":
                    manifest["files"]["snatch.exe"]["pe"]["subsystem"] = 2
                elif alteration == "schema_bool":
                    manifest["schema"] = True
                else:
                    manifest["files"]["LICENSE"]["size"] = True
                entries = dict(self.entries)
                entries["manifest.json"] = release.canonical(manifest)
                entries["manifest.sig"] = release.sign(entries["manifest.json"], self.key)
                self.reject(self.mutated(entries))

    def test_signed_duplicate_json_field_is_rejected(self):
        entries = dict(self.entries)
        raw = entries["manifest.json"].replace(b'{', b'{"schema":1,', 1)
        entries["manifest.json"], entries["manifest.sig"] = raw, release.sign(raw, self.key)
        self.reject(self.mutated(entries))

    def test_symlink_zip_member_is_rejected(self):
        path = self.folder / "symlink.zip"
        with zipfile.ZipFile(path, "w") as archive:
            for name, data in self.entries.items():
                info = zipfile.ZipInfo(name)
                info.create_system = 3
                info.external_attr = (stat.S_IFLNK | 0o777) << 16
                archive.writestr(info, data)
        self.reject(path)

    def test_zip_bomb_and_truncated_archive_are_rejected(self):
        entries = dict(self.entries)
        entries["manifest.json"] = b"x" * (release.MAX_META + 1)
        self.reject(self.mutated(entries))
        path = self.folder / "truncated.zip"
        path.write_bytes(self.archive.read_bytes()[:-20])
        self.reject(path)

    def test_failed_signing_preserves_previous_release(self):
        before = self.archive.read_bytes()
        with self.assertRaises(ValueError):
            release.package(self.payload, self.source, self.tools, "0.5.3", self.other_key, self.public, self.archive)
        self.assertEqual(before, self.archive.read_bytes())

    def test_pe_rejects_wrong_machine_subsystem_and_missing_mitigations(self):
        for offset, value in ((0x84, 0x14C), (0x98 + 68, 2), (0x98 + 70, 0)):
            data = bytearray(executable())
            struct.pack_into("<H", data, offset, value)
            with self.assertRaises(ValueError):
                release.pe_info(bytes(data), 3)
        for size in (0, 64, 200, 520):
            with self.assertRaises(ValueError):
                release.pe_info(executable()[:size], 3)

    def test_pe_rejects_dynamic_crt(self):
        data = bytearray(executable())
        data[576:593] = b"vcruntime140.dll\0"
        with self.assertRaises(ValueError):
            release.pe_info(bytes(data), 3)


if __name__ == "__main__":
    unittest.main()
