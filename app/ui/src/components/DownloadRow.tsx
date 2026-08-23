import { useEffect, useRef, useState } from "react";

import { api, errorMessage } from "../api/commands";
import { bytes, duration, kindOf, rate } from "../api/format";
import type { Row } from "../store/useDownloads";
import { DetailsDrawer } from "./DetailsDrawer";
import { ProgressBar } from "./ProgressBar";
import { StatusPill } from "./StatusPill";

const ICON: Record<string, string> = {
  video: "▶",
  audio: "♪",
  archive: "▤",
  app: "◈",
  doc: "▧",
  image: "▣",
  disk: "◉",
  file: "▪",
};

export function DownloadRow({
  row,
  onRefreshUrl,
  onChanged,
}: {
  row: Row;
  onRefreshUrl: (row: Row) => void;
  onChanged: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [menu, setMenu] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!menu) return;
    const close = (e: MouseEvent) => {
      if (!menuRef.current?.contains(e.target as Node)) setMenu(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, [menu]);

  const run = (f: () => Promise<unknown>) => async () => {
    setMenu(false);
    try {
      await f();
      setError(null);
      onChanged();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const live = row.live;
  const running = row.status === "active";
  const finished = row.status === "completed";

  // `128 MB / 1.2 GB · 24.3 MB/s · 45s left`
  const meta = [
    row.total_size ? `${bytes(row.bytes_done)} / ${bytes(row.total_size)}` : bytes(row.bytes_done),
    running && live ? rate(live.current_bps) : null,
    running && live?.eta_secs != null ? `${duration(live.eta_secs)} left` : null,
    running && live && live.conns > 0 ? `${live.conns} connections` : null,
    !running && !finished && row.queue_position != null
      ? `#${row.queue_position + 1} in queue`
      : null,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <div className="card">
      <div className="dl">
        <div className="kind" aria-hidden>
          {ICON[kindOf(row.filename)] ?? "▪"}
        </div>

        <div className="mid">
          <div className="name">
            <span title={row.filename}>{row.filename}</span>
            <StatusPill status={row.status} />
          </div>
          <div className="meta" title={row.host}>
            {meta || row.host}
          </div>
          {!finished && <ProgressBar done={row.bytes_done} total={row.total_size} status={row.status} />}
        </div>

        <div className="actions">
          {running && (
            <button className="icon-btn" title="Pause" onClick={run(() => api.pause(row.id))}>
              ❚❚
            </button>
          )}
          {(row.status === "paused" || row.status === "queued") && (
            <button className="icon-btn" title="Resume" onClick={run(() => api.resume(row.id))}>
              ▶
            </button>
          )}
          {(row.status === "failed" || row.status === "cancelled") && (
            <button className="icon-btn" title="Retry" onClick={run(() => api.retry(row.id))}>
              ↻
            </button>
          )}
          <button
            className="icon-btn"
            title={open ? "Hide details" : "Show details"}
            aria-expanded={open}
            onClick={() => setOpen((v) => !v)}
          >
            {open ? "⌃" : "⌄"}
          </button>

          <div className="menu-wrap" ref={menuRef}>
            <button className="icon-btn" title="More" onClick={() => setMenu((v) => !v)}>
              ⋮
            </button>
            {menu && (
              <div className="menu" role="menu">
                <button disabled={running || finished} onClick={run(() => api.resume(row.id))}>
                  Resume
                </button>
                <button disabled={!running} onClick={run(() => api.pause(row.id))}>
                  Pause
                </button>
                <button disabled={finished} onClick={run(() => api.cancel(row.id))}>
                  Cancel
                </button>
                <button disabled={finished} onClick={run(() => api.retry(row.id))}>
                  Retry
                </button>
                <button
                  onClick={() => {
                    setMenu(false);
                    onRefreshUrl(row);
                  }}
                >
                  Use New URL…
                </button>
                <hr />
                <button disabled={!finished} onClick={run(() => api.openFile(row.id))}>
                  Open File
                </button>
                <button onClick={run(() => api.revealInExplorer(row.id))}>Open Folder</button>
                <button onClick={() => { setMenu(false); setOpen(true); }}>Details</button>
                <hr />
                <button className="destructive" onClick={run(() => api.remove(row.id, false))}>
                  Remove from list
                </button>
                <button className="destructive" onClick={run(() => api.remove(row.id, true))}>
                  Delete file
                </button>
              </div>
            )}
          </div>
        </div>
      </div>

      {/* An expired link is an interruption, not a failure: everything downloaded is intact,
          and the only thing missing is a working address. The banner says exactly that. */}
      {row.status === "needs_attention" && (
        <div className="banner">
          <span className="grow">
            ⚠ Download link expired — {bytes(row.bytes_done)}
            {row.total_size ? ` of ${bytes(row.total_size)}` : ""} already downloaded and kept.
          </span>
          <button className="btn primary" onClick={() => onRefreshUrl(row)}>
            Provide New URL
          </button>
          <button className="btn" onClick={run(() => api.retry(row.id))}>
            Retry Original
          </button>
        </div>
      )}

      {row.status === "failed" && row.error && (
        <div className="banner bad">
          <span className="grow">{row.error}</span>
          <button className="btn" onClick={run(() => api.retry(row.id))}>
            Retry
          </button>
        </div>
      )}

      {error && <div className="banner bad">{error}</div>}

      {open && <DetailsDrawer id={row.id} live={row.live ?? null} />}
    </div>
  );
}
