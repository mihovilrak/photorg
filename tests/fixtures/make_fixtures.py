#!/usr/bin/env python3
"""Regenerates the committed EXIF fixtures.

Every file is a few hundred bytes and hand-built, so the test suite is
reproducible without shipping real camera output. Run from the repo root:

    python tests/fixtures/make_fixtures.py
"""
import os
import struct

HERE = os.path.dirname(os.path.abspath(__file__))

DATETIME_ORIGINAL = 0x9003
GPS_IFD = 0x8825
EXIF_IFD = 0x8769
MAKE = 0x010F
MODEL = 0x0110

ASCII, RATIONAL = 2, 5


def entry(tag, typ, count, value_or_offset):
    return struct.pack("<HHI", tag, typ, count) + struct.pack("<I", value_or_offset)


def ifd(entries, next_offset=0):
    out = struct.pack("<H", len(entries))
    for e in entries:
        out += e
    return out + struct.pack("<I", next_offset)


def dms(deg):
    """Degrees as three RATIONALs, the way cameras actually write GPS."""
    d = int(abs(deg))
    m = int((abs(deg) - d) * 60)
    s = (abs(deg) - d - m / 60) * 3600
    return struct.pack("<IIIIII", d, 1, m, 1, int(round(s * 100)), 100)


def build(datetime=None, gps=None, make=None, model=None):
    """Little-endian TIFF: IFD0 -> Exif IFD (+ GPS IFD), then a data blob."""
    tiff_header = b"II*\x00" + struct.pack("<I", 8)
    ifd0_entries = []
    exif_entries = []
    gps_entries = []
    blob = b""

    # Offsets are relative to the TIFF header, so lay the blob out last and
    # patch the pointers once the IFD sizes are known.
    ifd0_count = 1 + (1 if gps else 0) + (1 if make else 0) + (1 if model else 0)
    ifd0_size = 2 + 12 * ifd0_count + 4
    exif_off = 8 + ifd0_size
    exif_count = 1 if datetime else 0
    exif_size = 2 + 12 * exif_count + 4
    gps_off = exif_off + exif_size
    gps_count = 4 if gps else 0
    gps_size = 2 + 12 * gps_count + 4
    blob_off = gps_off + (gps_size if gps else 0)

    def stash(data):
        nonlocal blob
        off = blob_off + len(blob)
        blob += data
        return off

    if make:
        b = make.encode() + b"\0"
        ifd0_entries.append(entry(MAKE, ASCII, len(b), stash(b)))
    if model:
        b = model.encode() + b"\0"
        ifd0_entries.append(entry(MODEL, ASCII, len(b), stash(b)))
    ifd0_entries.append(entry(EXIF_IFD, 4, 1, exif_off))
    if gps:
        ifd0_entries.append(entry(GPS_IFD, 4, 1, gps_off))

    if datetime:
        b = datetime.encode() + b"\0"
        exif_entries.append(entry(DATETIME_ORIGINAL, ASCII, len(b), stash(b)))

    if gps:
        lat, lon, lat_ref, lon_ref = gps
        gps_entries.append(entry(1, ASCII, 2, int.from_bytes(lat_ref.encode() + b"\0\0\0", "little")))
        gps_entries.append(entry(2, RATIONAL, 3, stash(dms(lat))))
        gps_entries.append(entry(3, ASCII, 2, int.from_bytes(lon_ref.encode() + b"\0\0\0", "little")))
        gps_entries.append(entry(4, RATIONAL, 3, stash(dms(lon))))

    body = tiff_header + ifd(ifd0_entries) + ifd(exif_entries)
    if gps:
        body += ifd(gps_entries)
    body += blob
    return body


def jpeg(exif_body=None, trailing=b""):
    out = b"\xff\xd8"
    if exif_body is not None:
        app1 = b"Exif\x00\x00" + exif_body
        out += b"\xff\xe1" + struct.pack(">H", len(app1) + 2) + app1
    # A one-pixel-ish scan is unnecessary: nothing here decodes pixels.
    return out + trailing + b"\xff\xd9"


def webp(exif_body):
    chunk = b"EXIF" + struct.pack("<I", len(exif_body)) + exif_body
    if len(exif_body) % 2:
        chunk += b"\0"
    vp8x = b"VP8X" + struct.pack("<I", 10) + bytes([0x08, 0, 0, 0]) + b"\0\0\0\0\0\0"
    body = b"WEBP" + vp8x + chunk
    return b"RIFF" + struct.pack("<I", len(body)) + body


def heic(exif_body):
    """`ftyp` + a `meta`-less minimal box soup: enough to exercise the reader's
    failure path on a container whose EXIF support varies by crate version."""
    ftyp = struct.pack(">I", 24) + b"ftyp" + b"heic" + struct.pack(">I", 0) + b"heic" + b"mif1"
    payload = b"\0\0\0\0" + exif_body
    mdat = struct.pack(">I", len(payload) + 8) + b"mdat" + payload
    return ftyp + mdat


def box(boxtype, body):
    return struct.pack(">I", len(body) + 8) + boxtype + body


def isobmff(exif_body, major, compatible):
    """A HEIF/AVIF file with the real `meta` -> `iinf`/`iloc` -> `mdat` chain a
    phone writes: the Exif item is declared in an `infe`, located by an `iloc`
    extent, and stored in `mdat`. Nothing about the still image itself is
    present, because nothing here decodes pixels.
    """
    brands = b"".join(compatible)
    ftyp = box(b"ftyp", major + struct.pack(">I", 0) + brands)

    # The item payload is the APP1-style block real files store: a 4-byte
    # offset to the TIFF header, the Exif marker, then the TIFF block.
    item = struct.pack(">I", 6) + b"Exif\0\0" + exif_body

    hdlr = box(b"hdlr", struct.pack(">I", 0) + b"\0" * 4 + b"pict" + b"\0" * 13)
    infe = box(
        b"infe",
        bytes([2, 0, 0, 0])                  # version 2, flags 0
        + struct.pack(">HH", 1, 0)           # item_id 1, protection index 0
        + b"Exif"                            # item_type
        + b"Exif\0",                         # item_name
    )
    iinf = box(b"iinf", struct.pack(">I", 0) + struct.pack(">H", 1) + infe)

    def meta_with(offset):
        # iloc v0, 4-byte offsets and lengths, no base offset, one extent.
        iloc = box(
            b"iloc",
            struct.pack(">I", 0)
            + struct.pack(">H", 0x4400)
            + struct.pack(">H", 1)           # item_count
            + struct.pack(">H", 1)           # item_id
            + struct.pack(">H", 0)           # data_reference_index
            + struct.pack(">H", 1)           # extent_count
            + struct.pack(">II", offset, len(item)),
        )
        return box(b"meta", struct.pack(">I", 0) + hdlr + iinf + iloc)

    # Extents are absolute file offsets, and patching one does not change its
    # own size, so a throwaway pass is enough to learn where `mdat` lands.
    offset = len(ftyp) + len(meta_with(0)) + 8
    return ftyp + meta_with(offset) + box(b"mdat", item)


FILES = {
    "no_exif.jpg": jpeg(None),
    "truncated_exif.jpg": jpeg(build(datetime="2026:03:04 05:06:07")[:20]),
    "zero_date.jpg": jpeg(build(datetime="0000:00:00 00:00:00")),
    "dated.jpg": jpeg(build(datetime="2021:06:07 10:11:12", make="Canon", model="EOS R5")),
    "iso_week_boundary.jpg": jpeg(build(datetime="2026:12:31 23:59:59")),
    # Southern/western hemisphere: 33.87 S, 18.42 W of Greenwich is only correct
    # if the refs are applied, so a sign bug relocates this file entirely.
    "gps_south_west.jpg": jpeg(build(datetime="2026:05:06 07:08:09", gps=(33.9249, 18.4241, "S", "W"))),
    "gps_null_island.jpg": jpeg(build(datetime="2026:05:06 07:08:09", gps=(0.0, 0.0, "N", "E"))),
    "gps_zagreb.jpg": jpeg(build(datetime="2026:05:06 07:08:09", gps=(45.8150, 15.9819, "N", "E"))),
    "webp_dated.webp": webp(build(datetime="2019:02:03 04:05:06")),
    # Hand-built HEIF has no `meta`/`iloc` box tree, so the EXIF is
    # deliberately unreachable: this fixture pins the mtime fallback for a
    # container whose real-world support is verified against iPhone files.
    "heic_minimal.heic": heic(build(datetime="2018:11:12 13:14:15")),
    # Spec-shaped containers with a real box tree, which is what proves the
    # reader reaches EXIF inside ISOBMFF rather than falling back to the mtime.
    "heic_dated.heic": isobmff(
        build(datetime="2020:07:08 09:10:11", make="Apple", model="iPhone 12"),
        b"heic",
        [b"mif1", b"heic"],
    ),
    "avif_dated.avif": isobmff(
        build(datetime="2022:09:10 11:12:13"), b"avif", [b"avif", b"mif1", b"miaf"]
    ),
}

if __name__ == "__main__":
    for name, data in FILES.items():
        path = os.path.join(HERE, name)
        with open(path, "wb") as f:
            f.write(data)
        print(f"{name}: {len(data)} bytes")
