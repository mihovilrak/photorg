#!/usr/bin/env python3
"""Compile the GeoNames cities1000 dataset into the binary blob the geocoder
embeds (Plan §1, §8).

    python data/make_cities.py <source> data/cities.bin

`<source>` is either a GeoNames `cities1000.txt` export or the simplified
`lat,lon,name,admin1,admin2,cc` CSV that ships with the `reverse_geocoder`
crate. The raw text is ~7.8 MB, which alone blows the 8 MB binary budget; this
strips the unused columns, interns the region names, and writes the records in
k-d tree order so the runtime does no work at startup.

Data: GeoNames (https://geonames.org), CC-BY 4.0.

Format (little-endian):
    magic   "PGC1"                     4 bytes
    count   u32                        record count
    regions u32                        region-table entry count
    names   u32                        length of the name blob
    rbytes  u32                        length of the region blob
    records count * 17 bytes           lat f32, lon f32, cc [u8;2],
                                       region u16, name_off u32, name_len u8
    offsets (regions + 1) * u32        region blob offsets
    regions rbytes                     region names, UTF-8
    names   names bytes                city names, UTF-8
"""

import csv
import struct
import sys

MAGIC = b"PGC1"
RECORD = struct.Struct("<ff2sHIB")


def read(path):
    """Yield (lat, lon, name, admin1, cc) from either accepted input format."""
    if path.endswith(".txt"):
        with open(path, encoding="utf-8", newline="") as fh:
            for row in csv.reader(fh, delimiter="\t", quoting=csv.QUOTE_NONE):
                # GeoNames column order, see the readme of the dump.
                yield float(row[4]), float(row[5]), row[1], row[10], row[8]
        return

    with open(path, encoding="utf-8", newline="") as fh:
        for row in csv.DictReader(fh):
            yield (
                float(row["lat"]),
                float(row["lon"]),
                row["name"],
                row["admin1"],
                row["cc"],
            )


def kd_order(records, lo, hi, depth=0):
    """Reorder in place so the median of each range sits at its midpoint —
    the same implicit layout `geocode::nearest` walks."""
    if hi - lo <= 1:
        return
    mid = lo + (hi - lo) // 2
    records[lo:hi] = sorted(records[lo:hi], key=lambda r: r[depth % 2])
    kd_order(records, lo, mid, depth + 1)
    kd_order(records, mid + 1, hi, depth + 1)


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    source, target = sys.argv[1], sys.argv[2]

    records = []
    for lat, lon, name, admin1, cc in read(source):
        cc = (cc or "").strip()[:2].upper().ljust(2)
        name = (name or "").strip()
        if len(name.encode("utf-8")) > 255:
            continue
        records.append((lat, lon, name, (admin1 or "").strip(), cc))
    kd_order(records, 0, len(records))

    regions, region_ids = [], {}
    names, name_offsets = bytearray(), {}
    body = bytearray()
    for lat, lon, name, admin1, cc in records:
        region = region_ids.setdefault(admin1, len(regions))
        if region == len(regions):
            regions.append(admin1.encode("utf-8"))
        if region > 0xFFFF:
            sys.exit("more than 65535 distinct regions")

        encoded = name.encode("utf-8")
        offset = name_offsets.get(encoded)
        if offset is None:
            offset = len(names)
            name_offsets[encoded] = offset
            names += encoded
        body += RECORD.pack(lat, lon, cc.encode("ascii"), region, offset, len(encoded))

    offsets, blob = bytearray(), bytearray()
    for region in regions:
        offsets += struct.pack("<I", len(blob))
        blob += region
    offsets += struct.pack("<I", len(blob))

    with open(target, "wb") as out:
        out.write(MAGIC)
        out.write(struct.pack("<IIII", len(records), len(regions), len(names), len(blob)))
        out.write(body)
        out.write(offsets)
        out.write(blob)
        out.write(names)

    print(f"{len(records)} cities, {len(regions)} regions -> {target}")


if __name__ == "__main__":
    main()
