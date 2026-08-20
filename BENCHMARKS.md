# Benchmarks

Reproduce any row with:

```sh
cargo run --release --example bench -- <count> [work-dir] [--scan-only] [--keep]
```

`examples/bench.rs` writes `<count>` ~1 KB JPEGs carrying real EXIF, spread
over twelve years of capture dates and 500 files per directory, then times
scan, EXIF, plan, and copy separately and reports peak RSS after each. Peak RSS
is process-wide and monotonic, so each row's figure is the high-water mark up
to and including that stage.

## Machine

AMD Ryzen 7 3700U (4C/8T), 14 GB RAM, NVMe SSD, Windows 11 Home, Defender
real-time protection **on**, `--workers 4` (the default), `--dedup off`.

Defender matters more than anything in this crate: a plain single-threaded
Python loop writing 3,000 1 KB files reaches only 336 files/s on this machine
*with no `fsync` at all*. Every absolute copy number below is a floor set by
the AV filter driver, not by the code. Treat the copy column as a per-file
overhead measurement; it says nothing about bulk throughput on real photos,
which are 3–5 MB each and bandwidth-bound.

## Results

| files | scan | EXIF | plan | copy | peak RSS |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 0.05 s (20k/s) | 0.03 s (32k/s) | 0.04 s (25k/s) | 2.90 s (345/s) | 7 MB |
| 10,000 | 0.05 s (218k/s) | 1.87 s (5.4k/s) | 0.35 s (29k/s) | 92.1 s (109/s) | 13 MB |
| 100,000 | 2.13 s (47k/s) | 17.0 s (5.9k/s) | 10.8 s (9.3k/s) | 1089 s (92/s) | 67 MB |
| 1,000,000 | 4.41 s (227k/s) | 5868 s (170/s) | 40.8 s (24k/s) | not run | **609 MB** |

The million-file row was taken with `--scan-only`: the copy stage would have
run for three hours at the rate the 100k row sets, and the memory NFR is about
the plan, which is where the peak is.

## Notes

**Scan** is `jwalk` traversal only, and is never the bottleneck. The 10k row is
faster per file than the 100k row purely because the whole tree was still in
the page cache from generation.

**EXIF** settles at ~5.9k files/s across all cores — about 1.4 ms of CPU per
file, which is the parse itself, not I/O — but only while the files are in the
page cache. At a million files they are not: 1 M × 1 KB spread over 2,000
directories does not survive the hour it takes to generate, and the stage falls
to 170 files/s. That is a 35× cliff, and it is the AV filter driver plus a cold
NVMe read on every open, not parsing. Real photo trees hit this immediately,
since 3–5 MB files never fit the cache at any count.

**Plan** is not pure CPU: `resolve_conflict` stats the destination once per
file to decide between planning, skipping, and renaming. That stat is what the
per-file cost is, and it is why the 100k row is slower per file than the 10k
one — the destination tree grows past anything the metadata cache holds.

**Memory** grows at ~0.6 KB per scanned file and is essentially linear:
7 MB @ 1k, 13 MB @ 10k, 67 MB @ 100k, 609 MB @ 1M. The peak always lands at the
end of the plan stage, where `Vec<PhotoMetadata>` and `Vec<Operation>` are both
fully resident — the scan and EXIF stages sit at roughly half that. **609 MB
exceeds the 500 MB target**, which is what motivates spilling the plan
to disk rather than holding it.

## Storage

Every row above is on the machine's only volume, an internal NVMe SSD. The
SSD/HDD/network comparison has no hardware to run on here — no
second physical disk, no HDD, no remote host — and an SMB loopback share would
measure protocol overhead against the same NVMe, which answers a different
question. The benchmark takes the work directory as its second argument, so
the comparison is one command per device once one is attached:

```sh
cargo run --release --example bench -- 10000 D:\bench      # HDD
cargo run --release --example bench -- 10000 \\nas\share  # network
```

What the numbers here already imply: scan and EXIF are open-heavy and will
degrade roughly with per-open latency, so a spinning disk should cost most in
those two stages, and a network share most of all — a round trip per file
rather than a seek. Copy is the stage designed for it: `--workers` defaults to
4 rather than the core count precisely because concurrent sequential writes
thrash seeks on HDDs and shares, and on those devices the right
setting is likely 1–2.

**Copy** does a read, a temp write, an `fsync`, a rename, and an mtime set per
file. The `fsync` is deliberate: it is what makes an interrupted run leave no
half-written file in the destination. It costs roughly 30% over a plain write
on this machine.
