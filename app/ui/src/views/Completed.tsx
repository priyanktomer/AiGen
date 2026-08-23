import { useState } from "react";

import { api, errorMessage } from "../api/commands";
import { bytes, kindOf, timestamp } from "../api/format";
import type { Row } from "../store/useDownloads";

export function Completed({ rows, onChanged }: { rows: Row[]; onChanged: () => void }) {
  const [q, setQ] = useState("");
  const [error, setError] = useState<string | null>(null);

  const done = rows
    .filter((r) => r.status === "completed")
    .filter((r) => r.filename.toLowerCase().includes(q.toLowerCase()))
    .sort((a, b) => (b.completed_at ?? 0) - (a.completed_at ?? 0));

  const act = (f: () => Promise<unknown>) => async () => {
    try {
      await f();
      setError(null);
      onChanged();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  return (
    <>
      <div className="field">
        <input
          type="text"
          value={q}
          placeholder="Search finished downloads…"
          onChange={(e) => setQ(e.target.value)}
        />
      </div>

      {error && <div className="card banner bad">{error}</div>}

      {done.length === 0 ? (
        <div className="empty">
          <div className="big" aria-hidden>
            ✓
          </div>
          {q ? "Nothing matches that." : "Finished downloads appear here."}
        </div>
      ) : (
        done.map((r) => (
          <div className="card dl" key={r.id}>
            <div className="kind" aria-hidden>
              ✓
            </div>
            <div className="mid">
              <div className="name">{r.filename}</div>
              <div className="meta">
                {bytes(r.bytes_done)} · {kindOf(r.filename)} · finished {timestamp(r.completed_at)}
              </div>
            </div>
            <div className="actions">
              <button className="btn subtle" onClick={act(() => api.openFile(r.id))}>
                Open
              </button>
              <button className="btn subtle" onClick={act(() => api.revealInExplorer(r.id))}>
                Folder
              </button>
              <button className="btn subtle" onClick={act(() => api.remove(r.id, false))}>
                Forget
              </button>
            </div>
          </div>
        ))
      )}
    </>
  );
}
