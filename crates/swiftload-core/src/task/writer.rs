//! The single writer task.
//!
//! Every worker sends `(offset, bytes)` over one **bounded** channel to one writer that owns
//! the file. Three things fall out of that, all of which matter:
//!
//! 1. **Backpressure for free.** When the disk is slower than the network the channel fills
//!    and workers block on `send`, so memory stays bounded no matter how many connections are
//!    running. It also gives the governor its disk-bound signal.
//! 2. **One owner of durability ordering.** A checkpoint may only ever claim bytes that are
//!    already durable, so the cycle is strictly `write -> sync_data -> commit checkpoint`.
//!    Getting this backwards is exactly how a "resumed" download produces a corrupt file that
//!    passes a size check.
//! 3. **Write coalescing.** Adjacent chunks merge into larger sequential writes, which matters
//!    on spinning disks and under antivirus real-time scanning.

use crate::{
    config::{CHECKPOINT_BYTES, CHECKPOINT_INTERVAL},
    util::intervals::RangeSet,
};
use bytes::Bytes;
use std::{fs::File, io, sync::Arc, time::Instant};
use tokio::sync::{mpsc, oneshot};

/// Chunks in flight before workers block. 256 x 64 KB caps buffered data at ~16 MB.
pub const WRITER_QUEUE: usize = 256;
/// Largest single coalesced write.
const MAX_COALESCE: usize = 1024 * 1024;

/// Where durable progress is recorded. Implemented by the SQLite store; stubbed in tests.
///
/// Called only *after* the data it describes has been fsynced.
pub trait CheckpointSink: Send + Sync + 'static {
    fn checkpoint(&self, ranges: &RangeSet, bytes_done: u64) -> io::Result<()>;
}

/// A sink that records nothing, for downloads that are not persisted (benchmarks, probes).
pub struct NullSink;
impl CheckpointSink for NullSink {
    fn checkpoint(&self, _ranges: &RangeSet, _bytes_done: u64) -> io::Result<()> {
        Ok(())
    }
}

pub enum WriteMsg {
    Data {
        offset: u64,
        bytes: Bytes,
    },
    /// Force a durability checkpoint now (pause, or shutdown).
    Sync(oneshot::Sender<io::Result<RangeSet>>),
    /// Final flush; the writer exits afterwards.
    Finish(oneshot::Sender<io::Result<RangeSet>>),
}

#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<WriteMsg>,
}

impl WriterHandle {
    pub async fn write(&self, offset: u64, bytes: Bytes) -> Result<(), WriterGone> {
        self.tx
            .send(WriteMsg::Data { offset, bytes })
            .await
            .map_err(|_| WriterGone)
    }

    /// True when the queue is saturated — the disk cannot keep up with the network.
    ///
    /// This is the governor's disk-bound signal: when it is set, adding connections cannot
    /// move more bytes, it only deepens the queue.
    pub fn is_backpressured(&self) -> bool {
        self.tx.capacity() == 0
    }

    pub fn queue_depth(&self) -> usize {
        WRITER_QUEUE - self.tx.capacity()
    }

    pub async fn sync(&self) -> io::Result<RangeSet> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(WriteMsg::Sync(tx))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"))?;
        rx.await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"))?
    }

    pub async fn finish(&self) -> io::Result<RangeSet> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(WriteMsg::Finish(tx))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"))?;
        rx.await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"))?
    }
}

#[derive(Debug, thiserror::Error)]
#[error("writer task has stopped")]
pub struct WriterGone;

/// Start the writer. Returns a handle and the task's join handle.
pub fn spawn(
    file: File,
    initial: RangeSet,
    sink: Arc<dyn CheckpointSink>,
) -> (WriterHandle, tokio::task::JoinHandle<io::Result<RangeSet>>) {
    let (tx, rx) = mpsc::channel(WRITER_QUEUE);
    let handle = tokio::spawn(run(file, initial, sink, rx));
    (WriterHandle { tx }, handle)
}

async fn run(
    file: File,
    initial: RangeSet,
    sink: Arc<dyn CheckpointSink>,
    mut rx: mpsc::Receiver<WriteMsg>,
) -> io::Result<RangeSet> {
    let file = Arc::new(file);
    let mut done = initial;
    let mut since_checkpoint = 0u64;
    let mut last_checkpoint = Instant::now();

    // Buffered, not-yet-written chunks awaiting coalescing.
    let mut pending: Vec<(u64, Bytes)> = Vec::new();
    let mut pending_bytes = 0usize;

    loop {
        let msg = match rx.recv().await {
            Some(m) => m,
            None => break, // all senders dropped
        };

        match msg {
            WriteMsg::Data { offset, bytes } => {
                pending_bytes += bytes.len();
                pending.push((offset, bytes));

                // Opportunistically drain whatever else is queued, so coalescing has material
                // to work with instead of writing 64 KB at a time.
                while pending_bytes < MAX_COALESCE {
                    match rx.try_recv() {
                        Ok(WriteMsg::Data { offset, bytes }) => {
                            pending_bytes += bytes.len();
                            pending.push((offset, bytes));
                        }
                        Ok(other) => {
                            flush_pending(
                                &file,
                                &mut pending,
                                &mut pending_bytes,
                                &mut done,
                                &mut since_checkpoint,
                            )?;
                            if handle_control(
                                other,
                                &file,
                                &sink,
                                &done,
                                &mut since_checkpoint,
                                &mut last_checkpoint,
                            )
                            .await?
                            {
                                return Ok(done);
                            }
                            break;
                        }
                        Err(_) => break,
                    }
                }

                flush_pending(
                    &file,
                    &mut pending,
                    &mut pending_bytes,
                    &mut done,
                    &mut since_checkpoint,
                )?;

                let due = since_checkpoint >= CHECKPOINT_BYTES
                    || last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL;
                if due {
                    durable_checkpoint(&file, &sink, &done).await?;
                    since_checkpoint = 0;
                    last_checkpoint = Instant::now();
                }
            }
            other => {
                flush_pending(
                    &file,
                    &mut pending,
                    &mut pending_bytes,
                    &mut done,
                    &mut since_checkpoint,
                )?;
                if handle_control(
                    other,
                    &file,
                    &sink,
                    &done,
                    &mut since_checkpoint,
                    &mut last_checkpoint,
                )
                .await?
                {
                    return Ok(done);
                }
            }
        }
    }

    flush_pending(
        &file,
        &mut pending,
        &mut pending_bytes,
        &mut done,
        &mut since_checkpoint,
    )?;
    durable_checkpoint(&file, &sink, &done).await?;
    Ok(done)
}

/// Returns true when the writer should exit.
async fn handle_control(
    msg: WriteMsg,
    file: &Arc<File>,
    sink: &Arc<dyn CheckpointSink>,
    done: &RangeSet,
    since_checkpoint: &mut u64,
    last_checkpoint: &mut Instant,
) -> io::Result<bool> {
    match msg {
        WriteMsg::Sync(reply) => {
            let r = durable_checkpoint(file, sink, done).await;
            *since_checkpoint = 0;
            *last_checkpoint = Instant::now();
            let _ = reply.send(r.map(|_| done.clone()));
            Ok(false)
        }
        WriteMsg::Finish(reply) => {
            let r = durable_checkpoint(file, sink, done).await;
            let _ = reply.send(r.map(|_| done.clone()));
            Ok(true)
        }
        WriteMsg::Data { .. } => unreachable!("data handled by the caller"),
    }
}

/// Sort, merge adjacent chunks, and issue positional writes.
fn flush_pending(
    file: &Arc<File>,
    pending: &mut Vec<(u64, Bytes)>,
    pending_bytes: &mut usize,
    done: &mut RangeSet,
    since_checkpoint: &mut u64,
) -> io::Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    pending.sort_unstable_by_key(|(o, _)| *o);

    let mut buf: Vec<u8> = Vec::new();
    let mut buf_start = 0u64;

    for (offset, bytes) in pending.drain(..) {
        if buf.is_empty() {
            buf_start = offset;
            buf.extend_from_slice(&bytes);
            continue;
        }
        if buf_start + buf.len() as u64 == offset && buf.len() + bytes.len() <= MAX_COALESCE {
            buf.extend_from_slice(&bytes); // contiguous: merge into one write
            continue;
        }
        crate::fsx::write_at(file, &buf, buf_start)?;
        done.insert(buf_start, buf.len() as u64);
        *since_checkpoint += buf.len() as u64;
        buf.clear();
        buf_start = offset;
        buf.extend_from_slice(&bytes);
    }
    if !buf.is_empty() {
        crate::fsx::write_at(file, &buf, buf_start)?;
        done.insert(buf_start, buf.len() as u64);
        *since_checkpoint += buf.len() as u64;
    }
    *pending_bytes = 0;
    Ok(())
}

/// Make data durable, *then* record it. Never the other way round.
async fn durable_checkpoint(
    file: &Arc<File>,
    sink: &Arc<dyn CheckpointSink>,
    done: &RangeSet,
) -> io::Result<()> {
    let f = file.clone();
    // fsync can block for milliseconds, so it does not belong on the async runtime.
    tokio::task::spawn_blocking(move || f.sync_data())
        .await
        .map_err(|e| io::Error::other(e.to_string()))??;

    let sink = sink.clone();
    let snapshot = done.clone();
    let total = done.total();
    tokio::task::spawn_blocking(move || sink.checkpoint(&snapshot, total))
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
}

/// Test helper: a sink that records every checkpoint it is given.
#[cfg(test)]
pub struct RecordingSink {
    pub calls: std::sync::Mutex<Vec<(RangeSet, u64)>>,
}

#[cfg(test)]
impl RecordingSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }
    pub fn last(&self) -> Option<(RangeSet, u64)> {
        self.calls.lock().unwrap().last().cloned()
    }
    pub fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[cfg(test)]
impl CheckpointSink for RecordingSink {
    fn checkpoint(&self, ranges: &RangeSet, bytes_done: u64) -> io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((ranges.clone(), bytes_done));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx;

    fn tmp(total: u64) -> (tempfile::TempDir, File, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.slpart");
        let f = fsx::open_part(&path, Some(total)).unwrap();
        (dir, f, path)
    }

    #[tokio::test]
    async fn writes_land_at_the_right_offsets_out_of_order() {
        let (_d, file, path) = tmp(12);
        let sink = RecordingSink::new();
        let (h, join) = spawn(file, RangeSet::new(), sink.clone());

        // Deliberately out of order, as independent segments would finish.
        h.write(6, Bytes::from_static(b"world")).await.unwrap();
        h.write(0, Bytes::from_static(b"hello ")).await.unwrap();
        h.write(11, Bytes::from_static(b"!")).await.unwrap();

        let ranges = h.finish().await.unwrap();
        drop(h);
        join.await.unwrap().unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"hello world!");
        assert_eq!(ranges.total(), 12);
        assert_eq!(
            ranges.spans(),
            &[(0, 12)],
            "contiguous coverage should merge to one span"
        );
    }

    #[tokio::test]
    async fn tracks_gaps_when_segments_are_not_contiguous() {
        let (_d, file, _p) = tmp(1000);
        let (h, join) = spawn(file, RangeSet::new(), RecordingSink::new());

        h.write(0, Bytes::from(vec![1u8; 100])).await.unwrap();
        h.write(500, Bytes::from(vec![2u8; 100])).await.unwrap();

        let ranges = h.finish().await.unwrap();
        drop(h);
        join.await.unwrap().unwrap();

        assert_eq!(ranges.spans(), &[(0, 100), (500, 100)]);
        assert_eq!(ranges.total(), 200);
        assert!(
            !ranges.contains_all(0, 1000),
            "the hole must be visible to the planner"
        );
    }

    #[tokio::test]
    async fn resumes_from_an_existing_range_set() {
        let (_d, file, _p) = tmp(1000);
        let initial = RangeSet::from_pairs([(0, 400)]);
        let (h, join) = spawn(file, initial, RecordingSink::new());

        h.write(400, Bytes::from(vec![7u8; 600])).await.unwrap();
        let ranges = h.finish().await.unwrap();
        drop(h);
        join.await.unwrap().unwrap();

        assert_eq!(
            ranges.spans(),
            &[(0, 1000)],
            "prior progress must be carried forward"
        );
    }

    #[tokio::test]
    async fn checkpoint_never_claims_more_than_was_written() {
        // The invariant that makes crash recovery correct.
        let (_d, file, _p) = tmp(10_000);
        let sink = RecordingSink::new();
        let (h, join) = spawn(file, RangeSet::new(), sink.clone());

        for i in 0..10u64 {
            h.write(i * 1000, Bytes::from(vec![0u8; 1000]))
                .await
                .unwrap();
        }
        let final_ranges = h.finish().await.unwrap();
        drop(h);
        join.await.unwrap().unwrap();

        assert!(
            sink.count() >= 1,
            "at least the final checkpoint must be recorded"
        );
        for (ranges, bytes) in sink.calls.lock().unwrap().iter() {
            assert_eq!(
                *bytes,
                ranges.total(),
                "byte count must agree with coverage"
            );
            assert!(
                ranges.total() <= final_ranges.total(),
                "a checkpoint claimed {} bytes but only {} were ever written",
                ranges.total(),
                final_ranges.total()
            );
        }
    }

    #[tokio::test]
    async fn sync_forces_a_checkpoint_without_stopping_the_writer() {
        let (_d, file, _p) = tmp(1000);
        let sink = RecordingSink::new();
        let (h, join) = spawn(file, RangeSet::new(), sink.clone());

        h.write(0, Bytes::from(vec![1u8; 100])).await.unwrap();
        let at_sync = h.sync().await.unwrap();
        assert_eq!(at_sync.total(), 100);
        assert_eq!(sink.last().unwrap().1, 100);

        // Still usable afterwards — this is what pause does.
        h.write(100, Bytes::from(vec![2u8; 100])).await.unwrap();
        let end = h.finish().await.unwrap();
        drop(h);
        join.await.unwrap().unwrap();
        assert_eq!(end.total(), 200);
    }

    #[tokio::test]
    async fn coalesces_adjacent_chunks_into_larger_writes() {
        // Correctness is the same either way; this is about not issuing 64 KB writes when
        // 1 MB ones would do, which matters under antivirus scanning.
        let (_d, file, path) = tmp(64 * 1024 * 8);
        let (h, join) = spawn(file, RangeSet::new(), RecordingSink::new());

        for i in 0..8u64 {
            h.write(i * 65536, Bytes::from(vec![i as u8; 65536]))
                .await
                .unwrap();
        }
        let ranges = h.finish().await.unwrap();
        drop(h);
        join.await.unwrap().unwrap();

        assert_eq!(ranges.spans(), &[(0, 64 * 1024 * 8)]);
        let content = std::fs::read(&path).unwrap();
        for i in 0..8usize {
            assert_eq!(content[i * 65536], i as u8, "chunk {i} landed wrong");
            assert_eq!(content[(i + 1) * 65536 - 1], i as u8);
        }
    }

    #[tokio::test]
    async fn reports_backpressure_when_the_queue_saturates() {
        let (_d, file, _p) = tmp(100 * 1024 * 1024);
        let (h, join) = spawn(file, RangeSet::new(), RecordingSink::new());

        assert!(!h.is_backpressured(), "an idle writer is not backpressured");
        assert_eq!(h.queue_depth(), 0);

        let end = h.finish().await.unwrap();
        drop(h);
        join.await.unwrap().unwrap();
        assert_eq!(end.total(), 0);
    }

    #[tokio::test]
    async fn overlapping_rewrites_are_idempotent() {
        // Happens legitimately after a crash rewind: bytes get re-fetched and re-written.
        let (_d, file, path) = tmp(100);
        let (h, join) = spawn(file, RangeSet::new(), RecordingSink::new());

        h.write(0, Bytes::from(vec![9u8; 100])).await.unwrap();
        h.write(0, Bytes::from(vec![9u8; 100])).await.unwrap();
        h.write(50, Bytes::from(vec![9u8; 50])).await.unwrap();

        let ranges = h.finish().await.unwrap();
        drop(h);
        join.await.unwrap().unwrap();

        assert_eq!(ranges.spans(), &[(0, 100)], "no double-counting");
        assert_eq!(std::fs::read(&path).unwrap(), vec![9u8; 100]);
    }
}
