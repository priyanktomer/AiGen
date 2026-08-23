import { api, errorMessage } from "../api/commands";
import { bytes } from "../api/format";
import type { Priority } from "../api/bindings/Priority";
import type { Row } from "../store/useDownloads";
import { useState } from "react";

/**
 * What is waiting, and in what order.
 *
 * Reordering never pre-empts a running download: stopping a transfer that is already moving to
 * start another throws away an open connection and a warm ramp, and the user asked for this
 * one sooner, not for that one to be punished.
 */
export function Queue({ rows, onChanged }: { rows: Row[]; onChanged: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const queued = rows
    .filter((r) => r.status === "queued")
    .sort((a, b) => (a.queue_position ?? 0) - (b.queue_position ?? 0));
  const running = rows.filter((r) => r.status === "active");

  const act = (f: () => Promise<unknown>) => async () => {
    try {
      await f();
      setError(null);
      onChanged();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  if (queued.length === 0 && running.length === 0) {
    return (
      <div className="empty">
        <div className="big" aria-hidden>
          ≡
        </div>
        Nothing is waiting. Downloads you add beyond the simultaneous limit will queue here.
      </div>
    );
  }

  return (
    <>
      {error && <div className="card banner bad">{error}</div>}

      {running.length > 0 && (
        <>
          <div className="side-label" style={{ padding: "6px 4px" }}>
            Running now
          </div>
          {running.map((r) => (
            <div className="card dl" key={r.id}>
              <div className="mid">
                <div className="name">{r.filename}</div>
                <div className="meta">
                  {bytes(r.bytes_done)}
                  {r.total_size ? ` / ${bytes(r.total_size)}` : ""} · {r.host}
                </div>
              </div>
            </div>
          ))}
        </>
      )}

      {queued.length > 0 && (
        <>
          <div className="side-label" style={{ padding: "14px 4px 6px" }}>
            Waiting
          </div>
          {queued.map((r, i) => (
            <div className="card dl" key={r.id}>
              <div className="kind" aria-hidden>
                {i + 1}
              </div>
              <div className="mid">
                <div className="name">{r.filename}</div>
                <div className="meta">
                  {r.total_size ? bytes(r.total_size) : "size unknown"} · {r.host}
                </div>
              </div>
              <div className="actions">
                <select
                  aria-label={`Priority for ${r.filename}`}
                  defaultValue="normal"
                  onChange={(e) =>
                    void act(() => api.setPriority(r.id, e.target.value as Priority))()
                  }
                >
                  <option value="high">High</option>
                  <option value="normal">Normal</option>
                  <option value="low">Low</option>
                </select>
                <button className="btn" onClick={act(() => api.startNow(r.id))}>
                  Start next
                </button>
                <button className="btn subtle" onClick={act(() => api.cancel(r.id))}>
                  Remove
                </button>
              </div>
            </div>
          ))}
        </>
      )}
    </>
  );
}
