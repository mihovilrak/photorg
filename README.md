# photorg

A fast, offline CLI that sorts a pile of photos into dated (and optionally
place-named) folders. It plans the whole run before it touches anything, never
overwrites without being told to, and can be interrupted and resumed.

- **Two passes.** Everything is scanned and planned first, then executed. A
  dry run and a real run produce the same plan.
- **Offline.** No network at runtime, including reverse geocoding.
- **Non-destructive by default.** Copy, not move. Name collisions get a
  suffix. Overwriting requires `--force`.
- **Duplicate-aware.** Identical bytes are detected in the source set *and* at
  the destination, so a rerun does not fan out copies.

## Install

```sh
cargo install photorg                         # with offline geocoding (default)
cargo install photorg --no-default-features   # smaller, no location support
```

Requires Rust 1.85 or newer. Builds on Windows, macOS and Linux with no system
dependencies.

Tagged releases carry prebuilt binaries for six targets, a `SHA256SUMS` file,
and a Sigstore provenance attestation tying each tarball to the workflow run
that produced it:

```sh
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify photorg-<target>.tar.gz --repo <owner>/photorg
```

The binaries are not Authenticode-signed or notarized, so Windows SmartScreen
and macOS Gatekeeper will warn on first run.

## Quick start

```sh
# Copy everything into <dest>/2023/06-June/...
photorg ~/Pictures/unsorted ~/Pictures/organized

# See the plan without writing anything
photorg A B --dry-run

# Move instead of copy, one folder per ISO week
photorg A B --mode move --group week

# Add country/region folders under the date folders
photorg A B --location region

# Machine-readable output
photorg A B --json > plan.jsonl
```

The source and destination must not overlap. A destination inside the source
tree is refused before anything is created.

## CLI reference

```bash
photorg <SOURCE> <DEST> [OPTIONS]
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--group <year\|month\|week\|day\|adaptive>` | `month` | Folder granularity. |
| `--location [<country\|region\|city>]` | off; bare flag = `region` | Append place folders inside the date path. |
| `--template <STR>` | – | Explicit path template; overrides `--group` and `--location`. |
| `--mode <copy\|move>` | `copy` | Copy or move the originals. |
| `--dry-run` | off | Plan and report, write nothing. |
| `--on-conflict <skip\|rename\|overwrite>` | `rename` | What to do when a **different** file already holds the destination name. |
| `--force` | off | Required by `--on-conflict overwrite`. |
| `--dedup <size-hash\|off>` | `size-hash` | Duplicate detection strategy. |
| `--workers <N>` | `4` | Concurrent copies. See [Performance](#performance). |
| `--adaptive-threshold <N>` | `400` | Files per folder before `--group adaptive` splits a node. |
| `--json` | off | JSONL on stdout instead of human-readable lines. |
| `--resume <FILE>` | – | Skip operations already recorded in a journal. Implies `--journal <FILE>`. |
| `--journal <FILE>` | – | Append every completed operation to this file. |
| `--include-sidecars[=<bool>]` | `true` | Carry `.xmp`/`.aae` sidecars along with their stills. `false` ignores them entirely. |
| `--include-video` | off | Also organize standalone videos. |
| `--filename-dates[=<bool>]` | `true` | Read dates out of filenames when EXIF has none. |
| `--follow-symlinks` | off | Follow symlinks while scanning. Off by default: a loop never ends. |
| `-q, --quiet` | off | Errors only. Stdout records are still written. |
| `-v, --verbose` | – | Repeat for more detail (`-v` info, `-vv` debug, `-vvv` trace). |

### What gets picked up

| Kind | Extensions |
| --- | --- |
| Images | `jpg jpeg png tif tiff webp heic heif avif` |
| RAW | `nef arw dng orf raf cr2` |
| Sidecars | `xmp aae` |
| Video | `mov mp4 m4v avi mts 3gp mpg` |

Extensions are matched case-insensitively and then confirmed against the file's
signature, so a renamed `.txt` is not organized as a photo.

**Standalone videos are skipped unless `--include-video` is given.** A video
that shares a directory and file stem with a still (an iPhone Live Photo, for
example) is *not* standalone: it always travels with its still, `--include-video`
or not.

`--include-sidecars false` drops `.xmp`/`.aae` files at scan time. It does not
affect Live Photo movies.

### Companion grouping

Files that share a directory and a file stem (`IMG_0042.CR2`, `IMG_0042.JPG`,
`IMG_0042.xmp`, `IMG_0042.MOV`) form one group. The best-dated member decides
the date and location for all of them, and either the whole group lands in the
same folder or none of it moves. That is what keeps a RAW+JPEG pair together and
stops an edit sidecar from being orphaned.

### Conflicts and duplicates

These are different things and are reported differently:

- **Duplicate** means *identical bytes* – same size, then BLAKE3. Duplicates are
  skipped with `duplicate-of-source` or `duplicate-of-destination`.
- **Conflict** means a *different* file already occupies the destination name.
  `--on-conflict rename` (the default) writes `stem_1.ext`, `stem_2.ext`, …;
  `skip` leaves the source alone; `overwrite` replaces the destination and
  requires `--force`.

Collisions are resolved serially in the planner, so the same input always
produces the same output names.

## Templates

`--template` takes a path template. It renders **directories only** – the file
name is appended by the tool, and there is no `{filename}` variable.

```sh
photorg A B --template "{year}/{year}-{month:02}-{day:02}"
photorg A B --template "{country}/{region}/{year}"
```

| Variable | Example |
| --- | --- |
| `{year}` | `2023` |
| `{month}` | `6` |
| `{month_name}` | `June` |
| `{day}` | `7` |
| `{iso_year}` | `2023` |
| `{iso_week}` | `23` |
| `{country}` | `Croatia` |
| `{region}` | `Grad Zagreb` |
| `{city}` | `Zagreb` |
| `{camera_make}` | `Canon` |
| `{camera_model}` | `EOS R5` |

Numeric variables accept a zero-padding spec: `{month:02}` renders `06`. The
leading `0` is mandatory and the width is at most 9.

The `--group` presets are just templates:

| `--group` | Template |
| --- | --- |
| `year` | `{year}` |
| `month` | `{year}/{month:02}-{month_name}` |
| `week` | `{iso_year}/{iso_year}-W{iso_week:02}` |
| `day` | `{year}/{month:02}/{day:02}` |
| `adaptive` | chosen per node (see below) |

A template must be relative and must not contain `..`. Unknown variables, an
unclosed `{`, a bad padding spec, and an empty template are all rejected before
the run starts.

### Sanitization rules

Every rendered path component is sanitized the same way on every platform, so a
collection organized on Linux transfers to Windows unchanged:

- Unicode is normalized to **NFC** (macOS hands back NFD, which is different
  bytes on a case-sensitive filesystem even though it renders identically).
- `< > : " / \ | ? *` and control characters become `-`.
- Components are truncated to 100 bytes on a character boundary. File names are
  truncated in the stem, so the extension survives.
- Leading and trailing dots and spaces are stripped – Windows drops them
  silently, which would collapse two distinct names onto one.
- Windows reserved names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
  `LPT1`–`LPT9`) get a `_` prefix.
- An empty result becomes `unnamed`; a missing date becomes `unknown-date`.

On Windows the `\\?\` prefix is added at the syscall boundary for long paths and
stripped everywhere else, so messages and JSONL stay readable.

## Dates

Precedence, first hit wins:

1. EXIF `DateTimeOriginal`
2. EXIF `CreateDate`
3. Filename date (`--filename-dates`, on by default)
4. Filesystem mtime
5. Filesystem creation time, where available
6. EXIF `ModifyDate`
7. `unknown-date/`

`ModifyDate` sits last on purpose: editors rewrite it, so it says when the file
was last touched rather than when the photo was taken.

Recognized filename patterns:

```text
IMG_20230815_123456.jpg   20230815_123456.jpg   PXL_20220103_180000000.jpg
IMG-20200101-WA0001.jpg   Screenshot 2024-01-05 at 10.11.12.png
```

Dates outside `1990 <= year <= now + 1` are rejected as noise, and
`0000:00:00 00:00:00` is treated as absent rather than as year zero. Corrupt or
truncated EXIF falls through the chain; it never fails the file.

### Timezone policy

**EXIF timestamps are naive local wall-clock and are never converted to another
zone.** Filesystem timestamps are absolute UTC and *are* converted to the
machine's local time before bucketing. `OffsetTimeOriginal` is parsed and
recorded but does **not** shift the bucketing date.

This rule exists because without it two copies of the same photo would land in
different months depending on which link of the fallback chain won.

### Adaptive grouping: read this before using it

`--group adaptive` picks a granularity per node, recursively: a year holding
more than `--adaptive-threshold` files splits into months, and a month over the
threshold splits into weeks.

**Adaptive layout is not stable across runs.** Adding photos can push a node
over the threshold and reshuffle files that were already organized into a
different folder. For incremental imports into an existing library, use a fixed
`--group`.

## Location

Reverse geocoding is fully offline: an embedded GeoNames `cities1000` dataset
(~150k places, CC-BY) plus a k-d tree nearest-neighbour lookup. No network, no
API key, no rate limits, microseconds per query. Lookups are cached by
coordinate rounded to ~100 m, so a burst of shots from one spot costs one query.

The dataset ships as `data/cities.bin`, a packed blob built by
`data/make_cities.py` from a GeoNames `cities1000.txt` dump: unused columns
dropped, region names interned, coordinates narrowed to `f32`, and records
written in k-d tree order so lookups do no work at startup. It costs 3.7 MB of
the binary against 7.8 MB for the raw CSV. Rebuild it with:

```sh
python data/make_cities.py cities1000.txt data/cities.bin
```

**Precision, honestly:** the lookup returns the *nearest populated place*.
`{country}` and `{region}` are dependable; `{city}` may name a town some
distance away. That is why the default depth is `region`.

GPS handling: latitude and longitude refs are applied (dropping the sign would
put Zagreb in the southern hemisphere), ranges are validated, zero denominators
are guarded, and exactly `(0.0, 0.0)` is treated as absent: it is a
broken-writer sentinel, not Null Island. A photo without usable GPS falls back
to the date-only path; no `unknown-location` branch is created by default.

Built with `--no-default-features`, the location variables render empty and the
tool warns once.

## Output

Human-readable output goes to stderr (progress bar, warnings, the closing
summary); machine records go to stdout. That means `--json | jq` works while the
progress bar is still rendering, and `--quiet` silences stderr without touching
the records on stdout.

### JSONL schema

With `--json`, one object per file is written to stdout, followed by exactly one
summary object.

```json
{"source":"cam/IMG_0042.jpg","destination":"2021/06-June/IMG_0042.jpg","action":"copy","reason":"planned","bytes":184320}
{"source":"cam/IMG_0042 (1).jpg","destination":"2021/06-June/IMG_0042.jpg","action":"skip","reason":"duplicate-of-source","bytes":184320,"duplicate_of":"cam/IMG_0042.jpg"}
{"type":"summary","source_root":"/photos/in","dest_root":"/photos/out","dry_run":false,"scanned":2,"planned":1,"copied":1,"moved":0,"overwritten":0,"skipped":1,"duplicates":1,"renamed":0,"resumed":0,"unshippable":0,"failed":0,"bytes":184320,"cancelled":false}
```

Per-file record:

| Field | Notes |
| --- | --- |
| `source` | Path relative to the source root, forward slashes. |
| `destination` | Relative to the destination root. Omitted when there is no target. |
| `action` | `copy`, `move`, `skip`, `overwrite`. |
| `reason` | `planned`, `duplicate-of-source`, `duplicate-of-destination`, `renamed`, `existing-file`, `already-done`. |
| `bytes` | Size of the source file. |
| `duplicate_of` | Present on duplicate reasons: the file this one duplicates. |
| `error` | Present only when the operation failed. |

The summary object always has `type: "summary"` and carries `source_root`,
`dest_root`, `dry_run`, `scanned`, `planned`, `copied`, `moved`, `overwritten`,
`skipped`, `duplicates`, `renamed`, `resumed`, `unshippable`, `failed`, `bytes`,
`cancelled`, and `failures` (omitted when empty).

Dry runs emit exactly the same records as real runs.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Clean run; nothing failed. |
| `1` | Finished, but some files failed or the run was cancelled. |
| `2` | Never started: bad arguments, overlapping roots, an unwritable or missing directory, an invalid template, not enough free space, or `overwrite` without `--force`. |

## Interrupt and resume

The first Ctrl-C stops issuing new work and lets in-flight copies finish, then
prints the usual summary; a second one deletes any half-written temp file and
exits immediately. Either way the destination never keeps a partial `.jpg` –
files appear only once they are complete, under their final name. Pass `--journal <FILE>` to record every completed
operation, then `--resume <FILE>` to pick up where the run stopped. `--resume`
implies `--journal` with the same file, so a run interrupted twice still makes
progress. Resumed files still count as content already shipped, so their
duplicates are recognized rather than copied under a new name.

## Performance

Scanning and EXIF extraction run in parallel across all cores. Copying runs on
its own small bounded pool, because concurrent large sequential writes thrash
seeks and can halve throughput on the wrong storage.

| Destination | `--workers` |
| --- | --- |
| NVMe / SSD | `4` (default), up to `8` |
| Spinning disk (HDD) | `1` |
| SMB / NFS / network share | `1`, or `2` if the link is fast and the server is idle |
| USB stick / SD card | `1` |

`--workers 1` is the right answer whenever the destination has one head or one
pipe. It is usually *faster* there, not slower.

Measured throughput and memory per stage, and the harness that produces them,
are in [BENCHMARKS.md](BENCHMARKS.md).

## Platforms and CI

Tested on Linux, Windows, and macOS (x86_64 and ARM64). Every push runs the
full suite on native Linux, Windows, and macOS runners, because case folding,
Unicode normalization, path length limits, and symlink handling only behave
authentically on a real filesystem. `tests/platform.rs` asserts all four.

ARM64 Linux release binaries are cross-compiled with `cross` under QEMU. That
covers **builds only**: filesystem behaviour on ARM is not validated by
emulation, and is taken from the native runners of the same OS.

## License

MIT. The bundled GeoNames dataset is CC-BY.
