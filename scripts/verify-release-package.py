#!/usr/bin/env python3
"""Verify Latte Lens release archive contents and its SHA-256 sidecar."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import glob
import hashlib
import re
import stat
import struct
import tarfile
import zipfile
from pathlib import Path


LOCAL_ZIP_HEADER = struct.Struct("<I5H3I2H")
LOCAL_ZIP_SIGNATURE = 0x04034B50
CLASSIC_EOCD = struct.Struct("<I4H2IH")
CLASSIC_EOCD_SIGNATURE = 0x06054B50
MAX_ZIP_COMMENT = 0xFFFF
PATH_SEMANTICS_EXTRA_FIELDS = {
    0x0008: "extended language encoding",
    0x000D: "PKWARE Unix link metadata",
    0x7075: "Info-ZIP Unicode Path",
    0x756E: "ASi Unix link metadata",
}
UNSUPPORTED_STRUCTURE_EXTRA_FIELDS = {
    0x0001: "Zip64 metadata is outside the small release-package boundary",
}
DATA_DESCRIPTOR_SIGNATURE = 0x08074B50
MAX_CLASSIC_ZIP_SIZE = 0xFFFFFFFF


@dataclass(frozen=True)
class LocalZipRecord:
    name: str
    raw_name: bytes
    extra: bytes
    flags: int
    compression: int
    crc: int
    compressed_size: int
    file_size: int
    data_offset: int


def archive_name(path: Path) -> str:
    if path.name.endswith(".tar.gz"):
        return path.name[: -len(".tar.gz")]
    if path.suffix == ".zip":
        return path.stem
    raise ValueError(f"unsupported release archive: {path}")


def validate_member_name(name: str, *, directory: bool, zip_entry: bool) -> str:
    """Return a canonical member path or reject platform-ambiguous spelling."""
    if not name or "\0" in name:
        raise AssertionError(f"archive member has an empty or NUL-containing name: {name!r}")
    if "\\" in name:
        raise AssertionError(f"archive member uses an ambiguous backslash path: {name!r}")
    if name.startswith("/") or re.match(r"^[A-Za-z]:", name):
        raise AssertionError(f"archive member uses an absolute or drive-prefixed path: {name!r}")

    if directory and zip_entry:
        if not name.endswith("/"):
            raise AssertionError(f"ZIP directory name is not canonical: {name!r}")
        canonical = name[:-1]
    else:
        if name.endswith("/"):
            raise AssertionError(f"archive member has an unexpected trailing slash: {name!r}")
        canonical = name

    components = canonical.split("/")
    if any(component in ("", ".", "..") for component in components):
        raise AssertionError(f"archive member has a non-canonical or traversing path: {name!r}")
    if ":" in canonical:
        raise AssertionError(f"archive member has a drive or stream-ambiguous path: {name!r}")
    return canonical


def tar_files(path: Path, package: str, expected: set[str]) -> dict[str, int]:
    files: dict[str, int] = {}
    seen: set[str] = set()
    with tarfile.open(path, "r:gz") as archive:
        for member in archive.getmembers():
            name = validate_member_name(member.name, directory=member.isdir(), zip_entry=False)
            if name in seen:
                raise AssertionError(f"duplicate archive member: {member.name!r}")
            seen.add(name)

            if member.isdir():
                if name != package:
                    raise AssertionError(f"unexpected directory in {path}: {member.name!r}")
                continue
            if member.type not in (tarfile.REGTYPE, tarfile.AREGTYPE):
                raise AssertionError(
                    f"non-regular tar member in {path}: {member.name!r} (type {member.type!r})"
                )
            if name not in expected:
                raise AssertionError(f"unexpected file in {path}: {member.name!r}")
            files[name] = member.size
    return files


def validate_zip_extra(extra: bytes, *, member: str, source: str) -> None:
    """Reject malformed or path/type-aliasing ZIP extra-field streams."""
    offset = 0
    while offset < len(extra):
        remaining = len(extra) - offset
        if remaining < 4:
            raise AssertionError(
                f"malformed {source} ZIP extra field for {member!r}: "
                f"{remaining} trailing byte(s)"
            )
        field_id, field_size = struct.unpack_from("<HH", extra, offset)
        offset += 4
        field_end = offset + field_size
        if field_end > len(extra):
            raise AssertionError(
                f"malformed {source} ZIP extra field {field_id:#06x} for {member!r}: "
                f"declares {field_size} byte(s), only {len(extra) - offset} remain"
            )
        if field_id in PATH_SEMANTICS_EXTRA_FIELDS:
            raise AssertionError(
                f"{source} ZIP extra field can alias path or file type for {member!r}: "
                f"{PATH_SEMANTICS_EXTRA_FIELDS[field_id]} ({field_id:#06x})"
            )
        if field_id in UNSUPPORTED_STRUCTURE_EXTRA_FIELDS:
            raise AssertionError(
                f"unsupported {source} ZIP extra field for {member!r}: "
                f"{UNSUPPORTED_STRUCTURE_EXTRA_FIELDS[field_id]} ({field_id:#06x})"
            )
        offset = field_end


def local_zip_metadata(
    archive: zipfile.ZipFile,
    info: zipfile.ZipInfo,
) -> LocalZipRecord:
    """Read the local name and extras instead of trusting central metadata alone."""
    file = archive.fp
    if file is None:
        raise AssertionError("ZIP archive is unexpectedly closed")
    original_offset = file.tell()
    try:
        file.seek(info.header_offset)
        header = file.read(LOCAL_ZIP_HEADER.size)
        if len(header) != LOCAL_ZIP_HEADER.size:
            raise AssertionError(f"truncated local ZIP header for {info.orig_filename!r}")
        values = LOCAL_ZIP_HEADER.unpack(header)
        if values[0] != LOCAL_ZIP_SIGNATURE:
            raise AssertionError(f"invalid local ZIP header signature for {info.orig_filename!r}")
        flags = values[2]
        compression = values[3]
        crc = values[6]
        compressed_size = values[7]
        file_size = values[8]
        filename_size, extra_size = values[-2:]
        raw_name = file.read(filename_size)
        local_extra = file.read(extra_size)
        if len(raw_name) != filename_size or len(local_extra) != extra_size:
            raise AssertionError(f"truncated local ZIP metadata for {info.orig_filename!r}")
    finally:
        file.seek(original_offset)

    encoding = "utf-8" if flags & 0x800 else "cp437"
    try:
        local_name = raw_name.decode(encoding)
    except UnicodeDecodeError as error:
        raise AssertionError(
            f"local ZIP name is not valid {encoding}: {info.orig_filename!r}"
        ) from error
    return LocalZipRecord(
        name=local_name,
        raw_name=raw_name,
        extra=local_extra,
        flags=flags,
        compression=compression,
        crc=crc,
        compressed_size=compressed_size,
        file_size=file_size,
        data_offset=info.header_offset + LOCAL_ZIP_HEADER.size + filename_size + extra_size,
    )


def validate_zip_name_aliases(
    info: zipfile.ZipInfo,
    local: LocalZipRecord,
) -> None:
    """Require central, original, normalized, and local names to agree exactly."""
    if "\0" in info.orig_filename or "\0" in info.filename or "\0" in local.name:
        raise AssertionError(f"ZIP member contains a NUL name alias: {info.orig_filename!r}")
    if info.orig_filename != info.filename:
        raise AssertionError(
            f"ZIP member original and decoded names differ: "
            f"{info.orig_filename!r} != {info.filename!r}"
        )
    if local.name != info.orig_filename:
        raise AssertionError(
            f"ZIP local and central names differ: {local.name!r} != {info.orig_filename!r}"
        )
    if (local.flags ^ info.flag_bits) & 0x800:
        raise AssertionError(f"ZIP local and central filename encodings differ: {info.filename!r}")

    central_encoding = "utf-8" if info.flag_bits & 0x800 else "cp437"
    try:
        central_raw_name = info.orig_filename.encode(central_encoding)
    except UnicodeEncodeError as error:
        raise AssertionError(
            f"ZIP central name does not round-trip as {central_encoding}: {info.orig_filename!r}"
        ) from error
    if local.raw_name != central_raw_name:
        raise AssertionError(f"ZIP local and central raw names differ: {info.orig_filename!r}")


def read_zip_bytes(archive: zipfile.ZipFile, offset: int, size: int) -> bytes:
    file = archive.fp
    if file is None:
        raise AssertionError("ZIP archive is unexpectedly closed")
    original_offset = file.tell()
    try:
        file.seek(offset)
        contents = file.read(size)
    finally:
        file.seek(original_offset)
    if len(contents) != size:
        raise AssertionError(f"truncated ZIP record at offset {offset}")
    return contents


def validate_classic_eocd(
    archive: zipfile.ZipFile,
    infos: list[zipfile.ZipInfo],
    start_dir: int,
) -> None:
    """Require one classic, single-disk central-directory boundary."""
    file = archive.fp
    if file is None:
        raise AssertionError("ZIP archive is unexpectedly closed")
    original_offset = file.tell()
    try:
        file.seek(0, 2)
        archive_size = file.tell()
    finally:
        file.seek(original_offset)
    search_size = min(archive_size, CLASSIC_EOCD.size + MAX_ZIP_COMMENT)
    search_offset = archive_size - search_size
    tail = read_zip_bytes(archive, search_offset, search_size)
    relative_offset = tail.rfind(struct.pack("<I", CLASSIC_EOCD_SIGNATURE))
    if relative_offset < 0 or relative_offset + CLASSIC_EOCD.size > len(tail):
        raise AssertionError("classic ZIP end-of-central-directory record is missing")
    eocd_offset = search_offset + relative_offset
    (
        signature,
        disk_number,
        central_disk,
        disk_entries,
        total_entries,
        central_size,
        central_offset,
        comment_size,
    ) = CLASSIC_EOCD.unpack_from(tail, relative_offset)
    if signature != CLASSIC_EOCD_SIGNATURE:
        raise AssertionError("invalid ZIP end-of-central-directory signature")
    if eocd_offset + CLASSIC_EOCD.size + comment_size != archive_size:
        raise AssertionError("ZIP end-of-central-directory extent is ambiguous")
    if disk_number != 0 or central_disk != 0 or disk_entries != total_entries:
        raise AssertionError("multi-disk ZIP archives are outside the release-package boundary")
    if total_entries != len(infos):
        raise AssertionError("ZIP central-directory entry count is inconsistent")
    if central_offset == MAX_CLASSIC_ZIP_SIZE or central_size == MAX_CLASSIC_ZIP_SIZE:
        raise AssertionError("Zip64 central directories are outside the release-package boundary")
    if central_offset != start_dir or eocd_offset != start_dir + central_size:
        raise AssertionError("ZIP central-directory boundaries are inconsistent")


def validate_zip_record_extent(
    archive: zipfile.ZipFile,
    info: zipfile.ZipInfo,
    local: LocalZipRecord,
    boundary: int,
) -> None:
    """Prove one local record occupies exactly its bounded region.

    Release ZIPs contain a few small records. Classic 32-bit data descriptors
    are supported; Zip64, prepended executables, padding, and archive-level
    local records are rejected instead of being guessed or signature-scanned.
    """
    if local.flags != info.flag_bits:
        raise AssertionError(f"ZIP local and central flags differ: {info.filename!r}")
    if local.compression != info.compress_type:
        raise AssertionError(f"ZIP local and central compression differ: {info.filename!r}")
    if info.compress_size > MAX_CLASSIC_ZIP_SIZE or info.file_size > MAX_CLASSIC_ZIP_SIZE:
        raise AssertionError(f"Zip64 record is outside the release-package boundary: {info.filename!r}")
    if local.compressed_size == MAX_CLASSIC_ZIP_SIZE or local.file_size == MAX_CLASSIC_ZIP_SIZE:
        raise AssertionError(f"Zip64 local sizes are unsupported: {info.filename!r}")

    uses_descriptor = bool(local.flags & 0x08)
    if uses_descriptor:
        if local.crc not in (0, info.CRC):
            raise AssertionError(f"ZIP local CRC disagrees with its descriptor: {info.filename!r}")
        if local.compressed_size not in (0, info.compress_size):
            raise AssertionError(f"ZIP local compressed size disagrees: {info.filename!r}")
        if local.file_size not in (0, info.file_size):
            raise AssertionError(f"ZIP local file size disagrees: {info.filename!r}")
    else:
        if local.crc != info.CRC:
            raise AssertionError(f"ZIP local and central CRC differ: {info.filename!r}")
        if local.compressed_size != info.compress_size:
            raise AssertionError(f"ZIP local and central compressed sizes differ: {info.filename!r}")
        if local.file_size != info.file_size:
            raise AssertionError(f"ZIP local and central file sizes differ: {info.filename!r}")

    data_end = local.data_offset + info.compress_size
    if data_end > boundary:
        raise AssertionError(f"overlapping or truncated ZIP local record: {info.filename!r}")
    trailing_size = boundary - data_end
    if not uses_descriptor:
        if trailing_size != 0:
            raise AssertionError(
                f"unindexed local record or gap after ZIP member: {info.filename!r}"
            )
        return

    if trailing_size == 12:
        crc, compressed_size, file_size = struct.unpack(
            "<III",
            read_zip_bytes(archive, data_end, trailing_size),
        )
    elif trailing_size == 16:
        signature, crc, compressed_size, file_size = struct.unpack(
            "<IIII",
            read_zip_bytes(archive, data_end, trailing_size),
        )
        if signature != DATA_DESCRIPTOR_SIGNATURE:
            raise AssertionError(f"invalid ZIP data descriptor signature: {info.filename!r}")
    else:
        raise AssertionError(
            f"unsupported descriptor or unindexed local record after: {info.filename!r}"
        )
    if (crc, compressed_size, file_size) != (info.CRC, info.compress_size, info.file_size):
        raise AssertionError(f"ZIP data descriptor disagrees with central entry: {info.filename!r}")


def validate_zip_record_bijection(
    archive: zipfile.ZipFile,
    infos: list[zipfile.ZipInfo],
) -> dict[int, LocalZipRecord]:
    if not infos:
        raise AssertionError("release ZIP has no local file records")
    start_dir = getattr(archive, "start_dir", None)
    if not isinstance(start_dir, int) or start_dir <= 0:
        raise AssertionError("ZIP central-directory boundary is invalid")
    validate_classic_eocd(archive, infos, start_dir)

    offsets = [info.header_offset for info in infos]
    if len(set(offsets)) != len(offsets):
        raise AssertionError("multiple ZIP central entries reference the same local record")
    ordered = sorted(infos, key=lambda info: info.header_offset)
    records: dict[int, LocalZipRecord] = {}
    cursor = 0
    for index, info in enumerate(ordered):
        if info.header_offset != cursor:
            raise AssertionError(
                f"unindexed, overlapping, or prepended ZIP local record before: {info.filename!r}"
            )
        boundary = ordered[index + 1].header_offset if index + 1 < len(ordered) else start_dir
        if boundary <= info.header_offset or boundary > start_dir:
            raise AssertionError(f"invalid ZIP local record boundary: {info.filename!r}")
        local = local_zip_metadata(archive, info)
        validate_zip_record_extent(archive, info, local, boundary)
        records[info.header_offset] = local
        cursor = boundary
    if cursor != start_dir:
        raise AssertionError("ZIP local records do not end at the central directory")
    return records


def zip_files(path: Path, package: str, expected: set[str]) -> dict[str, int]:
    files: dict[str, int] = {}
    seen: set[str] = set()
    with zipfile.ZipFile(path) as archive:
        infos = archive.infolist()
        local_records = validate_zip_record_bijection(archive, infos)
        for info in infos:
            local = local_records[info.header_offset]
            validate_zip_name_aliases(info, local)
            validate_zip_extra(
                info.extra,
                member=info.orig_filename,
                source="central",
            )
            validate_zip_extra(
                local.extra,
                member=info.orig_filename,
                source="local",
            )

            unix_mode = (info.external_attr >> 16) & 0xFFFF
            unix_type = stat.S_IFMT(unix_mode)
            if unix_type not in (0, stat.S_IFREG, stat.S_IFDIR):
                raise AssertionError(
                    f"non-regular ZIP member in {path}: {info.filename!r} "
                    f"(mode {unix_mode:#o})"
                )

            has_directory_suffix = info.filename.endswith("/")
            has_dos_directory_flag = bool(info.external_attr & 0x10)
            is_directory = (
                has_directory_suffix
                or has_dos_directory_flag
                or unix_type == stat.S_IFDIR
            )
            name = validate_member_name(
                info.filename,
                directory=is_directory,
                zip_entry=True,
            )
            if name in seen:
                raise AssertionError(f"duplicate archive member: {info.filename!r}")
            seen.add(name)

            if is_directory:
                if not has_directory_suffix:
                    raise AssertionError(f"ZIP directory name is not canonical: {info.filename!r}")
                if unix_type == stat.S_IFREG:
                    raise AssertionError(f"ZIP directory is marked as a regular file: {info.filename!r}")
                if name != package:
                    raise AssertionError(f"unexpected directory in {path}: {info.filename!r}")
                continue
            if unix_type == stat.S_IFDIR or has_dos_directory_flag:
                raise AssertionError(f"ZIP file is marked as a directory: {info.filename!r}")
            if info.flag_bits & 0x1:
                raise AssertionError(f"encrypted ZIP member is not supported: {info.filename!r}")
            if name not in expected:
                raise AssertionError(f"unexpected file in {path}: {info.filename!r}")
            files[name] = info.file_size

        corrupt = archive.testzip()
        if corrupt is not None:
            raise AssertionError(f"ZIP member failed its CRC check: {corrupt!r}")
    return files


def archive_files(path: Path, package: str, expected: set[str]) -> dict[str, int]:
    if path.name.endswith(".tar.gz"):
        return tar_files(path, package, expected)

    if path.suffix == ".zip":
        return zip_files(path, package, expected)

    raise ValueError(f"unsupported release archive: {path}")


def verify_checksum(path: Path) -> str:
    checksum_path = Path(f"{path}.sha256")
    if not checksum_path.is_file():
        raise AssertionError(f"missing checksum sidecar: {checksum_path}")

    fields = checksum_path.read_text(encoding="ascii").strip().split(maxsplit=1)
    if len(fields) != 2 or not re.fullmatch(r"[0-9a-fA-F]{64}", fields[0]):
        raise AssertionError(f"invalid checksum format in {checksum_path}")

    referenced_name = fields[1].lstrip("*")
    if referenced_name != path.name:
        raise AssertionError(
            f"checksum references {referenced_name!r}, expected {path.name!r}"
        )

    expected = fields[0].lower()
    actual = hashlib.sha256(path.read_bytes()).hexdigest()
    if actual != expected:
        raise AssertionError(f"checksum mismatch for {path}: {actual} != {expected}")
    return actual


def expand_archives(patterns: list[str]) -> list[Path]:
    paths = {
        Path(match)
        for pattern in patterns
        for match in glob.glob(pattern)
    }
    if not paths:
        raise AssertionError(f"no release archives matched: {', '.join(patterns)}")
    return sorted(paths)


def verify_archive(path: Path, binary: str) -> None:
    package = archive_name(path)
    expected = {
        f"{package}/{binary}",
        f"{package}/README.md",
        f"{package}/LICENSE",
    }
    sizes = archive_files(path, package, expected)
    files = set(sizes)
    if files != expected:
        raise AssertionError(
            f"unexpected contents in {path}: got {sorted(files)}, expected {sorted(expected)}"
        )
    if sizes[f"{package}/{binary}"] <= 0:
        raise AssertionError(f"packaged binary is empty in {path}")

    digest = verify_checksum(path)
    print(f"Verified archive: {path}")
    print(f"Contents: {', '.join(sorted(files))}")
    print(f"SHA-256: {digest}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("archives", nargs="+", help="archive paths or glob patterns")
    parser.add_argument("--binary", required=True, help="expected packaged binary name")
    args = parser.parse_args()

    for path in expand_archives(args.archives):
        verify_archive(path, args.binary)


if __name__ == "__main__":
    main()
