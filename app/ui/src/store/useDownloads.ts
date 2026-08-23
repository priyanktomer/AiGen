import { useCallback, useEffect, useRef, useState } from "react";

import { api } from "../api/commands";
import { onNotice, onProgress, onStateChanged } from "../api/events";
import type { DownloadSummary } from "../api/bindings/DownloadSummary";
import type { Notice } from "../api/bindings/Notice";
import type { ProgressSnapshot } from "../api/bindings/ProgressSnapshot";

export type Row = DownloadSummary & { live?: ProgressSnapshot };

/**
 * The download list, and the live numbers laid over it.
 *
 * Two sources, deliberately kept apart. The store is authoritative about *what exists* and is
 * re-read whenever a status changes; progress ticks are a transient overlay that never write
 * back. A dropped tick therefore shows slightly stale numbers for 250 ms instead of leaving
 * the list permanently disagreeing with the database.
 */
export function useDownloads() {
  const [rows, setRows] = useState<DownloadSummary[]>([]);
  const [live, setLive] = useState<Record<string, ProgressSnapshot>>({});
  const [notices, setNotices] = useState<Notice[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setRows(await api.listDownloads());
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  // A burst of status changes should cost one re-read, not one per event.
  const pending = useRef<number | null>(null);
  const scheduleRefresh = useCallback(() => {
    if (pending.current !== null) return;
    pending.current = window.setTimeout(() => {
      pending.current = null;
      void refresh();
    }, 80);
  }, [refresh]);

  useEffect(() => {
    void refresh();
    const unlisteners = [
      onProgress((batch) => {
        setLive((prev) => {
          const next = { ...prev };
          for (const p of batch) next[p.id] = p;
          return next;
        });
      }),
      onStateChanged(() => scheduleRefresh()),
      onNotice((n) => {
        setNotices((prev) => [...prev.filter((p) => !sameNotice(p, n)), n]);
        scheduleRefresh();
      }),
    ];
    return () => {
      for (const u of unlisteners) void u.then((f) => f());
      if (pending.current !== null) window.clearTimeout(pending.current);
    };
  }, [refresh, scheduleRefresh]);

  // Drop live readings for downloads that are no longer running, so a paused row does not keep
  // showing the speed it had at the moment it stopped.
  const merged: Row[] = rows.map((r) => {
    const l = live[r.id];
    return r.status === "active" && l ? { ...r, live: l, bytes_done: l.bytes_done } : r;
  });

  const dismissNotice = useCallback((n: Notice) => {
    setNotices((prev) => prev.filter((p) => !sameNotice(p, n)));
  }, []);

  return { rows: merged, notices, dismissNotice, refresh, error };
}

function sameNotice(a: Notice, b: Notice): boolean {
  return a.kind === b.kind && a.id === b.id;
}
