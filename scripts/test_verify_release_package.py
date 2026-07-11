#!/usr/bin/env python3
"""Adversarial tests for the release archive verifier."""

from __future__ import annotations

import hashlib
import io
import importlib.util
import stat
import struct
import sys
import tarfile
import tempfile
import unittest
import warnings
import zipfile
import zlib
from pathlib import Path

_VERIFIER_PATH = Path(__file__).with_name("verify-release-package.py")
_VERIFIER_SPEC = importlib.util.spec_from_file_location("verify_release_package", _VERIFIER_PATH)
assert _VERIFIER_SPEC is not None and _VERIFIER_SPEC.loader is not None
verifier = importlib.util.module_from_spec(_VERIFIER_SPEC)
sys.modules[_VERIFIER_SPEC.name] = verifier
_VERIFIER_SPEC.loader.exec_module(verifier)


PACKAGE = "latte-lens-0.1.0-test-target"


class ReleasePackageVerifierTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary_directory.name)

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def write_checksum(
        self,
        archive: Path,
        *,
        digest: str | None = None,
        reference: str | None = None,
    ) -> None:
        digest = digest or hashlib.sha256(archive.read_bytes()).hexdigest()
        reference = reference or archive.name
        Path(f"{archive}.sha256").write_text(
            f"{digest}  {reference}\n",
            encoding="ascii",
        )

    def replace_local_extra_id(
        self,
        archive: Path,
        member_name: str,
        field_id: int,
    ) -> None:
        contents = bytearray(archive.read_bytes())
        with zipfile.ZipFile(archive) as source:
            member = source.getinfo(member_name)
            values = verifier.LOCAL_ZIP_HEADER.unpack_from(contents, member.header_offset)
        filename_size, extra_size = values[-2:]
        self.assertGreaterEqual(extra_size, 4)
        extra_offset = member.header_offset + verifier.LOCAL_ZIP_HEADER.size + filename_size
        struct.pack_into("<H", contents, extra_offset, field_id)
        archive.write_bytes(contents)
        self.write_checksum(archive)

    def append_unindexed_local_record(self, archive: Path) -> None:
        donor_stream = io.BytesIO()
        with zipfile.ZipFile(donor_stream, "w", compression=zipfile.ZIP_DEFLATED) as donor:
            donor.writestr("unindexed-record", b"not part of the package schema")
        donor_contents = donor_stream.getvalue()
        with zipfile.ZipFile(io.BytesIO(donor_contents)) as donor:
            donor_local_record = donor_contents[: donor.start_dir]

        contents = bytearray(archive.read_bytes())
        with zipfile.ZipFile(archive) as source:
            central_offset = source.start_dir
            expected_entries = len(source.infolist())
        contents[central_offset:central_offset] = donor_local_record
        eocd_offset = contents.rfind(b"PK\x05\x06")
        self.assertGreaterEqual(eocd_offset, 0)
        self.assertEqual(struct.unpack_from("<H", contents, eocd_offset + 10)[0], expected_entries)
        struct.pack_into(
            "<I",
            contents,
            eocd_offset + 16,
            central_offset + len(donor_local_record),
        )
        archive.write_bytes(contents)
        self.write_checksum(archive)

    @staticmethod
    def tar_member(name: str, data: bytes = b"data", member_type: bytes | None = None) -> tuple[tarfile.TarInfo, bytes | None]:
        member = tarfile.TarInfo(name)
        member.type = member_type or tarfile.REGTYPE
        if member.type in (tarfile.REGTYPE, tarfile.AREGTYPE):
            member.size = len(data)
            return member, data
        return member, None

    @staticmethod
    def zip_member(
        name: str,
        data: bytes = b"data",
        mode: int = stat.S_IFREG | 0o644,
        *,
        create_system: int = 3,
        extra: bytes = b"",
    ) -> tuple[zipfile.ZipInfo, bytes]:
        member = zipfile.ZipInfo(name)
        member.create_system = create_system
        member.extra = extra
        if create_system == 0:
            member.external_attr = 0x10 if stat.S_ISDIR(mode) else 0x20
        else:
            member.external_attr = mode << 16
            if stat.S_ISDIR(mode):
                member.external_attr |= 0x10
        return member, data

    def build_tar(
        self,
        *,
        binary: bytes = b"executable",
        include_license: bool = True,
        include_root: bool = True,
        extras: list[tuple[tarfile.TarInfo, bytes | None]] | None = None,
    ) -> Path:
        archive = self.root / f"{PACKAGE}.tar.gz"
        members = [
            self.tar_member(f"{PACKAGE}/latte-lens", binary),
            self.tar_member(f"{PACKAGE}/README.md", b"readme"),
        ]
        if include_root:
            members.insert(0, self.tar_member(PACKAGE, member_type=tarfile.DIRTYPE))
        if include_license:
            members.append(self.tar_member(f"{PACKAGE}/LICENSE", b"license"))
        members.extend(extras or [])
        with tarfile.open(archive, "w:gz", format=tarfile.PAX_FORMAT) as output:
            for member, data in members:
                output.addfile(member, None if data is None else _BytesReader(data))
        self.write_checksum(archive)
        return archive

    def build_zip(
        self,
        *,
        binary: bytes = b"MZ executable",
        binary_member: tuple[zipfile.ZipInfo, bytes] | None = None,
        create_system: int = 3,
        include_license: bool = True,
        include_root: bool = True,
        extras: list[tuple[zipfile.ZipInfo, bytes]] | None = None,
    ) -> Path:
        archive = self.root / f"{PACKAGE}.zip"
        members = [
            binary_member
            or self.zip_member(
                f"{PACKAGE}/latte-lens.exe",
                binary,
                create_system=create_system,
            ),
            self.zip_member(
                f"{PACKAGE}/README.md",
                b"readme",
                create_system=create_system,
            ),
        ]
        if include_root:
            members.insert(
                0,
                self.zip_member(
                    f"{PACKAGE}/",
                    b"",
                    stat.S_IFDIR | 0o755,
                    create_system=create_system,
                ),
            )
        if include_license:
            members.append(
                self.zip_member(
                    f"{PACKAGE}/LICENSE",
                    b"license",
                    create_system=create_system,
                )
            )
        members.extend(extras or [])
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
                for member, data in members:
                    output.writestr(member, data)
        self.write_checksum(archive)
        return archive

    def build_data_descriptor_zip(self) -> Path:
        archive = self.root / f"{PACKAGE}.zip"
        stream = _UnseekableBytesIO()
        members = [
            self.zip_member(
                f"{PACKAGE}/",
                b"",
                stat.S_IFDIR | 0o755,
                create_system=0,
            ),
            self.zip_member(
                f"{PACKAGE}/latte-lens.exe",
                b"MZ executable",
                create_system=0,
            ),
            self.zip_member(f"{PACKAGE}/README.md", b"readme", create_system=0),
            self.zip_member(f"{PACKAGE}/LICENSE", b"license", create_system=0),
        ]
        with zipfile.ZipFile(stream, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for member, data in members:
                output.writestr(member, data)
        archive.write_bytes(stream.getvalue())
        self.write_checksum(archive)
        return archive

    def assert_tar_rejected(self, extras: list[tuple[tarfile.TarInfo, bytes | None]]) -> None:
        with self.assertRaises(AssertionError):
            verifier.verify_archive(self.build_tar(extras=extras), "latte-lens")

    def assert_zip_rejected(self, extras: list[tuple[zipfile.ZipInfo, bytes]]) -> None:
        with self.assertRaises(AssertionError):
            verifier.verify_archive(self.build_zip(extras=extras), "latte-lens.exe")

    def test_valid_tar_and_zip_are_accepted(self) -> None:
        verifier.verify_archive(self.build_tar(), "latte-lens")
        verifier.verify_archive(self.build_zip(), "latte-lens.exe")
        verifier.verify_archive(self.build_tar(include_root=False), "latte-lens")
        verifier.verify_archive(
            self.build_zip(include_root=False),
            "latte-lens.exe",
        )

    def test_canonical_fat_windows_zip_metadata_is_accepted(self) -> None:
        allowed_timestamp = struct.pack("<HHBI", 0x5455, 5, 1, 0)
        binary = self.zip_member(
            f"{PACKAGE}/latte-lens.exe",
            b"MZ executable",
            create_system=0,
            extra=allowed_timestamp,
        )
        verifier.verify_archive(
            self.build_zip(binary_member=binary, create_system=0),
            "latte-lens.exe",
        )

    def test_canonical_classic_data_descriptors_are_accepted(self) -> None:
        archive = self.build_data_descriptor_zip()
        with zipfile.ZipFile(archive) as source:
            self.assertTrue(all(member.flag_bits & 0x08 for member in source.infolist()))
        verifier.verify_archive(archive, "latte-lens.exe")

    def test_unindexed_local_record_is_rejected(self) -> None:
        archive = self.build_zip(create_system=0)
        self.append_unindexed_local_record(archive)
        with zipfile.ZipFile(archive) as source:
            self.assertEqual(
                {member.filename for member in source.infolist() if not member.is_dir()},
                {
                    f"{PACKAGE}/latte-lens.exe",
                    f"{PACKAGE}/README.md",
                    f"{PACKAGE}/LICENSE",
                },
            )
        with self.assertRaises(AssertionError):
            verifier.verify_archive(archive, "latte-lens.exe")

    def test_zip64_metadata_outside_release_boundary_is_rejected(self) -> None:
        zip64_extra = struct.pack("<HHQ", 0x0001, 8, 123)
        binary = self.zip_member(
            f"{PACKAGE}/latte-lens.exe",
            b"MZ executable",
            extra=zip64_extra,
        )
        with self.assertRaises(AssertionError):
            verifier.verify_archive(
                self.build_zip(binary_member=binary),
                "latte-lens.exe",
            )

    def test_raw_nul_filename_alias_is_rejected(self) -> None:
        expected = f"{PACKAGE}/latte-lens.exe"
        raw_name = f"{expected}\0x"
        member, data = self.zip_member(expected, b"MZ executable")
        member.filename = raw_name
        member.orig_filename = raw_name
        archive = self.build_zip(binary_member=(member, data))

        with zipfile.ZipFile(archive) as source:
            parsed = source.infolist()[1]
            self.assertEqual(parsed.filename, expected)
            self.assertEqual(parsed.orig_filename, raw_name)
        with self.assertRaises(AssertionError):
            verifier.verify_archive(archive, "latte-lens.exe")

    def test_unicode_path_traversal_alias_is_rejected(self) -> None:
        expected = f"{PACKAGE}/latte-lens.exe"
        original = expected.encode("utf-8")
        alias = b"../../escape.exe"
        payload = b"\x01" + struct.pack("<I", zlib.crc32(original)) + alias
        unicode_path = struct.pack("<HH", 0x7075, len(payload)) + payload
        binary = self.zip_member(
            expected,
            b"MZ executable",
            extra=unicode_path,
        )
        archive = self.build_zip(binary_member=binary)

        with zipfile.ZipFile(archive) as source:
            parsed = source.infolist()[1]
            self.assertIn(struct.pack("<H", 0x7075), parsed.extra)
            self.assertIn(alias, parsed.extra)
        with self.assertRaises(AssertionError):
            verifier.verify_archive(archive, "latte-lens.exe")

    def test_local_only_unicode_path_traversal_alias_is_rejected(self) -> None:
        expected = f"{PACKAGE}/latte-lens.exe"
        original = expected.encode("utf-8")
        alias = b"../../escape.exe"
        payload = b"\x01" + struct.pack("<I", zlib.crc32(original)) + alias
        central_only_placeholder = struct.pack("<HH", 0xCAFE, len(payload)) + payload
        binary = self.zip_member(
            expected,
            b"MZ executable",
            extra=central_only_placeholder,
        )
        archive = self.build_zip(binary_member=binary)
        self.replace_local_extra_id(archive, expected, 0x7075)

        with zipfile.ZipFile(archive) as source:
            parsed = source.getinfo(expected)
            self.assertIn(struct.pack("<H", 0xCAFE), parsed.extra)
        with self.assertRaises(AssertionError):
            verifier.verify_archive(archive, "latte-lens.exe")

    def test_malformed_zip_extra_field_is_rejected(self) -> None:
        binary = self.zip_member(
            f"{PACKAGE}/latte-lens.exe",
            b"MZ executable",
            extra=b"\xff",
        )
        with self.assertRaises(AssertionError):
            verifier.verify_archive(
                self.build_zip(binary_member=binary),
                "latte-lens.exe",
            )

    def test_missing_expected_files_are_rejected(self) -> None:
        with self.assertRaises(AssertionError):
            verifier.verify_archive(
                self.build_tar(include_license=False),
                "latte-lens",
            )
        with self.assertRaises(AssertionError):
            verifier.verify_archive(
                self.build_zip(include_license=False),
                "latte-lens.exe",
            )

    def test_extra_files_are_rejected(self) -> None:
        self.assert_tar_rejected([self.tar_member(f"{PACKAGE}/extra.txt")])
        self.assert_zip_rejected([self.zip_member(f"{PACKAGE}/extra.txt")])

    def test_symlinks_and_hardlinks_are_rejected(self) -> None:
        symlink, _ = self.tar_member(f"{PACKAGE}/link", member_type=tarfile.SYMTYPE)
        symlink.linkname = "../../outside"
        hardlink, _ = self.tar_member(f"{PACKAGE}/hardlink", member_type=tarfile.LNKTYPE)
        hardlink.linkname = f"{PACKAGE}/latte-lens"
        self.assert_tar_rejected([(symlink, None)])
        self.assert_tar_rejected([(hardlink, None)])

        zip_symlink = self.zip_member(
            f"{PACKAGE}/link",
            b"../../outside",
            stat.S_IFLNK | 0o777,
        )
        self.assert_zip_rejected([zip_symlink])

    def test_traversal_and_absolute_paths_are_rejected(self) -> None:
        for name in (f"{PACKAGE}/../escape", "/absolute/escape"):
            self.assert_tar_rejected([self.tar_member(name)])
            self.assert_zip_rejected([self.zip_member(name)])

    def test_backslash_and_drive_prefixed_paths_are_rejected(self) -> None:
        for name in (f"{PACKAGE}\\escape", "C:/escape"):
            self.assert_tar_rejected([self.tar_member(name)])
            self.assert_zip_rejected([self.zip_member(name)])

    def test_dot_and_empty_path_components_are_rejected(self) -> None:
        for name in (f"./{PACKAGE}/escape", f"{PACKAGE}//escape"):
            self.assert_tar_rejected([self.tar_member(name)])
            self.assert_zip_rejected([self.zip_member(name)])

    def test_duplicates_are_rejected(self) -> None:
        self.assert_tar_rejected([self.tar_member(f"{PACKAGE}/README.md", b"second")])
        self.assert_zip_rejected([self.zip_member(f"{PACKAGE}/README.md", b"second")])

    def test_unexpected_directories_are_rejected(self) -> None:
        self.assert_tar_rejected(
            [self.tar_member(f"{PACKAGE}/nested", member_type=tarfile.DIRTYPE)]
        )
        self.assert_zip_rejected(
            [self.zip_member(f"{PACKAGE}/nested/", b"", stat.S_IFDIR | 0o755)]
        )

    def test_fifo_and_device_members_are_rejected(self) -> None:
        for member_type in (
            tarfile.FIFOTYPE,
            tarfile.CHRTYPE,
            tarfile.BLKTYPE,
            tarfile.CONTTYPE,
            tarfile.GNUTYPE_SPARSE,
        ):
            self.assert_tar_rejected(
                [self.tar_member(f"{PACKAGE}/special", member_type=member_type)]
            )
        self.assert_zip_rejected(
            [self.zip_member(f"{PACKAGE}/fifo", b"", stat.S_IFIFO | 0o644)]
        )
        self.assert_zip_rejected(
            [self.zip_member(f"{PACKAGE}/device", b"", stat.S_IFCHR | 0o644)]
        )

    def test_zero_byte_binaries_are_rejected(self) -> None:
        with self.assertRaises(AssertionError):
            verifier.verify_archive(self.build_tar(binary=b""), "latte-lens")
        with self.assertRaises(AssertionError):
            verifier.verify_archive(self.build_zip(binary=b""), "latte-lens.exe")

    def test_bad_checksum_digest_and_filename_are_rejected(self) -> None:
        for archive, binary in (
            (self.build_tar(), "latte-lens"),
            (self.build_zip(), "latte-lens.exe"),
        ):
            self.write_checksum(archive, digest="0" * 64)
            with self.assertRaises(AssertionError):
                verifier.verify_archive(archive, binary)
            self.write_checksum(archive, reference=f"other-{archive.name}")
            with self.assertRaises(AssertionError):
                verifier.verify_archive(archive, binary)


class _BytesReader:
    def __init__(self, data: bytes) -> None:
        self.data = data
        self.offset = 0

    def read(self, size: int = -1) -> bytes:
        if size < 0:
            size = len(self.data) - self.offset
        chunk = self.data[self.offset : self.offset + size]
        self.offset += len(chunk)
        return chunk


class _UnseekableBytesIO(io.BytesIO):
    def seekable(self) -> bool:
        return False

    def seek(self, offset: int, whence: int = 0) -> int:
        raise io.UnsupportedOperation("stream is intentionally unseekable")


if __name__ == "__main__":
    unittest.main()
