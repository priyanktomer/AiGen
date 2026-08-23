# SwiftLoad benchmark

64.0 MB per run, 3 repetitions per treatment, interleaved. Throughput is the median of per-run rates, with p10/p90 to show spread.

## per-connection cap

_each connection is capped, so parallelism should scale nearly linearly_

| conns | median | p10 | p90 | vs. 1 conn | peak conns | TCP conns | CPU ms | peak RSS |
|---|---|---|---|---|---|---|---|---|
| 1 | 3.8 MB/s | 3.8 MB/s | 3.8 MB/s | 1.00x | 1 | 2 | 350 | 71.4 MB |
| 8 | 31.7 MB/s | 31.6 MB/s | 31.7 MB/s | 8.27x | 8 | 9 | 210 | 71.4 MB |
| 16 | 65.8 MB/s | 65.3 MB/s | 65.8 MB/s | 17.17x | 16 | 17 | 190 | 71.4 MB |
| auto | 36.1 MB/s | 31.5 MB/s | 36.1 MB/s | 9.42x | 16 | 17 | 230 | 71.4 MB |
| auto-warm | 65.7 MB/s | 65.6 MB/s | 65.8 MB/s | 17.15x | 16 | 17 | 200 | 71.4 MB |

## shared total cap

_one budget is shared, so extra connections should buy nothing_

| conns | median | p10 | p90 | vs. 1 conn | peak conns | TCP conns | CPU ms | peak RSS |
|---|---|---|---|---|---|---|---|---|
| 1 | 15.6 MB/s | 15.3 MB/s | 15.6 MB/s | 1.00x | 1 | 2 | 230 | 71.4 MB |
| 8 | 15.3 MB/s | 15.3 MB/s | 15.3 MB/s | 0.98x | 8 | 9 | 256 | 71.4 MB |
| 16 | 15.3 MB/s | 15.3 MB/s | 15.3 MB/s | 0.98x | 16 | 17 | 283 | 71.4 MB |
| auto | 15.2 MB/s | 15.2 MB/s | 15.3 MB/s | 0.98x | 8 | 9 | 263 | 71.4 MB |
| auto-warm | 15.2 MB/s | 15.2 MB/s | 15.2 MB/s | 0.98x | 8 | 9 | 256 | 71.4 MB |

## unshaped loopback

_no network bottleneck at all, so this measures overhead, not speed_

| conns | median | p10 | p90 | vs. 1 conn | peak conns | TCP conns | CPU ms | peak RSS |
|---|---|---|---|---|---|---|---|---|
| 1 | 389.3 MB/s | 380.9 MB/s | 398.4 MB/s | 1.00x | 1 | 2 | 160 | 71.4 MB |
| 8 | 371.2 MB/s | 352.2 MB/s | 376.1 MB/s | 0.95x | 8 | 10 | 190 | 71.4 MB |
| 16 | 361.2 MB/s | 322.9 MB/s | 361.2 MB/s | 0.93x | 16 | 17 | 206 | 71.4 MB |
| auto | 381.3 MB/s | 378.8 MB/s | 385.7 MB/s | 0.98x | 4 | 6 | 176 | 71.4 MB |
| auto-warm | 388.9 MB/s | 382.7 MB/s | 391.3 MB/s | 1.00x | 4 | 7 | 170 | 71.4 MB |

## What this shows

- **Where segmentation wins.** Against a per-connection cap, 16 connections reached 17.17x the throughput of 1. This is the case that makes a download manager worth having: the server, not the link, is the limit.
- **Where it does not.** Against a shared cap, 8 connections reached 0.98x the throughput of 1 — that is, essentially nothing. Extra connections here cost CPU, memory and server load for no gain, which is why the default connection count is conservative and the governor stops when it detects this.
- **Adaptive, second download from a known host.** Reusing what the previous download learned, the governor reached 1.00x the best fixed level. Exploration is not free — discovering that a server allows sixteen useful connections costs most of a short download — so remembering the answer is worth more than exploring faster.
- **Adaptive vs. the best fixed choice.** The governor reached 0.55x the throughput of the best fixed level tested (16 connections), without being told the connection count, **below the 10% bar the project sets for itself**. That is the cost of exploring: the governor has to try a level before it can know it is better, and on a download this short the trying is most of the transfer. The warm row above is the same governor once it has something to remember. The bar for adaptive concurrency is not that it wins, but that it never loses badly to a sensible fixed guess.

### Caveats

- These runs are against a **local** server over loopback, so there is no real network. That is deliberate: the shaping is ground truth, so the per-connection and shared-cap results mean exactly what they say. It also means the unshaped row measures syscall and copy overhead, not download speed.
- Real-world results depend on the server, the path, and the time of day. Nothing here predicts what any particular download will do.
- 3 repetitions per treatment, interleaved rather than blocked. Blocked runs conflate the treatment with whatever else the machine was doing.
- No claim is made that SwiftLoad is faster than any other downloader on any particular file. What is claimed is that it detects which of these regimes it is in, and adjusts.
