# NTFS artifact compression: copy-only spike

Research and experiment date: 2026-10-15. This is an exploratory result,
not a production compression policy or a replacement for heli's GC.

## Finding

Transparent compression reduced the reported allocation of two copied heli
debug artifacts while preserving their logical bytes and modification times.
WOF LZX saved 78.77% across this executable/PDB pair; ordinary NTFS compression
saved 54.59%. Those percentages describe these two files, not a Cargo target
directory, the FLUI cache, or a sustained workload.

This is a useful alternative to investigate where deleting cold artifacts
would remove live runtime assets. It preserves paths and readable content,
but writes can remove WOF compression and restore the original allocation.
Compression therefore cannot provide a hard disk budget or replace the
runtime safety interlock.

## Scope and reproducibility

Only copies from `E:\heli\target\multi-project-tool\debug` were changed.
Every run created a fresh UUID-named directory below
`E:\heli\target\ntfs-compression-spike`. No source tree, original executable,
original PDB, or production build cache was compressed. No builds were run.

The local reproduction script and full machine-readable evidence are:

- [run.ps1](../../target/ntfs-compression-spike/run.ps1)
- [results.json](../../target/ntfs-compression-spike/results.json)

Run from this workspace with PowerShell 7:

```powershell
pwsh -NoProfile -File E:\heli\target\ntfs-compression-spike\run.ps1
```

The script calls `compact.exe /C /F <own-copy>` and
`compact.exe /C /F /EXE:LZX <own-copy>`. It hashes each file before and after,
measures allocation with `GetCompressedFileSizeW`, enumerates data streams,
records `LastWriteTimeUtc`, and executes the copied executable with
`--version`. It also tests a second copy opened for writing, explicit WOF
decompression, and compression of copies whose executable image is running.
It never invokes `/CompactOs` or changes directory inheritance flags.

The environment reported Windows `10.0.26300.0` and PowerShell `7.6.5`.
`.NET DriveInfo` reported both D: and E: as NTFS. The copy/compression tests
completed without requesting elevation. A separate optional hard-link probe
was denied by the sandbox and its pending escalation was cancelled; no
hard-link result is claimed.

## Measurements

Allocation below means the file allocation reported by the Win32 API, not
measured volume space freed. Logical sizes stayed unchanged in every row.

| Own artifact copy | Logical/baseline allocated bytes | NTFS `/C` allocated bytes | WOF `/EXE:LZX` allocated bytes |
|---|---:|---:|---:|
| `cargo-heli.exe` | 10,086,912 | 5,636,096 (44.12% smaller) | 2,789,376 (72.35% smaller) |
| `cargo_heli.pdb` | 60,788,736 | 26,550,272 (56.32% smaller) | 12,255,232 (79.84% smaller) |
| Pair | 70,875,648 | 32,186,368 (54.59% smaller) | 15,044,608 (78.77% smaller) |

All four compact operations returned 0. Each executable compression variant
returned 0 from `--version` and printed `cargo-heli 0.0.0`.

SHA-256 remained identical before/after both compression formats:

```text
cargo-heli.exe  846A5D465EB66E4D19631592AFDE232DE818DE530A05A2BEDB6B3D5C209A7D20
cargo_heli.pdb  361AC8394DEF8B93FA94D7DB74F046610CCA74652382D517A8C4DE120B2B5E72
```

`LastWriteTimeUtc` was preserved exactly, including subsecond precision:

| Artifact | Before and after NTFS compression | Before and after WOF compression |
|---|---|---|
| Executable | `2026-10-15T21:56:14.4063241Z` | `2026-10-15T21:56:14.4063241Z` |
| PDB | `2026-10-15T21:56:14.5085537Z` | `2026-10-15T21:56:14.5085537Z` |

The final run's compact wall times were approximately 91 ms/582 ms for
NTFS executable/PDB and 262 ms/1,044 ms for WOF executable/PDB. These are
individual observations, not a controlled throughput or Cargo latency
benchmark. Full compile, link, test, debugger and warm-read performance was
not measured.

### Write and decompression controls

The independent WOF executable copy initially occupied 2,789,376 bytes.
Opening it read/write and writing back its unchanged first byte restored
allocation to 10,086,912 bytes: an increase of 7,297,536 bytes. The logical
length and SHA-256 stayed unchanged, and `--version` still returned 0.
This control deliberately changes the write timestamp; timestamp preservation
is asserted for compression, not for a later write.

An additional copy compressed with LZX then explicitly uncompressed using
`compact /U /EXE` returned to 10,086,912 bytes with its original SHA-256;
both commands returned 0. Intermediate peak allocation during conversion
was not sampled, so no transient space requirement is inferred.

### Running-image control

Each own executable copy was running as heli's internal `lease-run` parent
of a six-second sleeping child when compression was attempted. The process
was alive at the attempt; both guardian/child runs completed with exit 0.

- Ordinary NTFS compression returned 1 with a sharing-violation diagnostic.
  Allocation and hash stayed unchanged.
- WOF LZX returned 0 and reduced allocation from 10,086,912 to 2,789,376
  bytes; the hash stayed unchanged.

This is evidence for these mapped images on this host, not permission to
compress arbitrary in-use caches. A production candidate should select
closed cold files, skip sharing violations and unsupported files, and keep
the runtime/GC protections until broader workload validation exists.

## What Microsoft documents

`/EXE` targets files read frequently and rarely modified; LZX is its most
compact algorithm. Ordinary `/C` can mark a directory so new files inherit
NTFS compression, whereas `/EXE` does not do that. `/U /EXE` and `/U` remove
the corresponding different compression formats. A future tool must inspect
per-file results, rather than treating an attempted command as proof of
compression. [Microsoft compact reference](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/compact)

Microsoft explicitly documents automatic decompression when a WOF file is
opened for writing; ordinary NTFS compressed files remain compressed through
writes. This supports comparing ordinary NTFS for writable cold intermediates
with WOF for read-mostly or immutable artifacts. Neither choice is accepted
for heli production by this small spike.
[Microsoft Windows compression guidance](https://learn.microsoft.com/en-us/windows/iot/iot-enterprise/optimize/compactos)

The individual-file WOF API requires Windows 10/Server 2016 or later.
LZX uses 32 KiB chunks; XPRESS4K favors inexpensive computation and rapid
access. Those are different performance tradeoffs, so LZX's observed size
advantage must be evaluated against actual Cargo I/O and compile latency.
[WOF compression information](https://learn.microsoft.com/en-us/windows/win32/api/wofapi/ns-wofapi-wof_file_compression_info_v1)

WOF can report compression as not beneficial; savings are not guaranteed
for every file. [WofSetFileDataLocation](https://learn.microsoft.com/en-us/windows/win32/api/wofapi/nf-wofapi-wofsetfiledatalocation)

## Accounting and operational limits

- Heli's logical-size accounting cannot observe these savings: each file's
  length is unchanged. A physical accounting proposal needs separate
  allocated-byte measurements and consistent policy semantics.
- `GetCompressedFileSizeW` documents actual storage for compressed and sparse
  files. Its documented error handling distinguishes `INVALID_FILE_SIZE`
  from a valid low DWORD and combines the high DWORD for 64-bit sizes.
  The sample's returned values matched compact's reported stored bytes.
  [Microsoft GetCompressedFileSizeW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getcompressedfilesizew)
- WOF's implementation uses a sparse default stream and compressed storage
  associated with `WofCompressedData`. The Win32 enumeration in this sample
  exposed only the default stream; its allocation query already reflected
  the compressed size. Do not assume an independently enumerable ADS should
  be added again. Broader files/streams and filesystem versions need tests.
  [Microsoft's WOF implementation explanation](https://devblogs.microsoft.com/oldnewthing/20190618-00/?p=102597)
- Allocation reported per pathname is not a proof of independently owned
  blocks or independently reclaimable space. Hard-link identity, aliases,
  sparse regions and filesystem metadata need explicit accounting. The
  optional local hard-link experiment was blocked, so this spike makes no
  deduplication or hard-link reclamation claim. Volume free-space deltas were
  not used because unrelated host activity would confound them.
- Retain free-space headroom for write-triggered decompression and conversion;
  the measured write control immediately consumed another 7.30 MB. Concurrent
  writers can erase compression savings without increasing logical size.
- A maintenance policy would need low-priority/background work, bounded I/O,
  a closed-file selection rule, per-file error recovery, and reapplication
  after rewrites. Their cost and Cargo freshness behavior need a real cache
  experiment; this spike only establishes byte/mtime preservation on copies.
- Regular NTFS compression has filesystem and size constraints distinct from
  WOF; Microsoft's API documents no ReFS support and a 30 GB uncompressed-file
  limit for `FSCTL_SET_COMPRESSION`.
  [Microsoft FSCTL_SET_COMPRESSION](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_set_compression)
- Linux/macOS need their own mechanisms and measurements. These Windows NTFS
  numbers do not transfer to APFS, ReFS, Btrfs, or an uncompressed filesystem.

Compression is a complementary retention experiment. The next acceptance
step would measure representative real Cargo artifacts, mixed writes and
reads, compiler/debugger/test latency and physical-space accounting under
concurrent activity. Until then, no production compression or change to
the global runtime guard is justified by these two-file savings.
