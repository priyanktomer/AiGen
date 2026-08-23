import { useEffect, useRef, useState } from "react";

import { api, errorMessage } from "../api/commands";
import { onConnections } from "../api/events";
import { bytes, rate, timestamp } from "../api/format";
import type { ConnectionSnapshot } from "../api/bindings/ConnectionSnapshot";
import type { DownloadDetails } from "../api/bindings/DownloadDetails";
import type { ProgressSnapshot } from "../api/bindings/ProgressSnapshot";
import { ConnectionTable } from "./ConnectionTable";
import { Sparkline } from "./Sparkline";

type Tab = "connections" | "server" | "stats" | "links";

const RANGE_SUPPORT: Record<string, string> = {
  supported: "Yes — verified with a real ranged request",
  unsupported: "No",
  lied: "Advertised, then ignored — segmentation disabled for safety",
  unknown: "Not established",
};

/**
 * Advanced detail, behind a chevron.
 *
 * Opening this is what subscribes to the connection stream, and closing it unsubscribes. The
 * table is the engine's largest payload, so nobody pays for it unless they are looking at it.
 */
export function DetailsDrawer({ id, live }: { id: string; live: ProgressSnapshot | null }) {
  const [tab, setTab] = useState<Tab>("connections");
  const [details, setDetails] = useState<DownloadDetails | null>(null);
  const [conns, setConns] = useState<ConnectionSnapshot[]>([]);
  const [fullUrl, setFullUrl] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Short histories for the charts. Kept in a ref so a tick does not re-render the whole row.
  const speeds = useRef<number[]>([]);
  const concurrency = useRef<number[]>([]);
  const [, bump] = useState(0);

  useEffect(() => {
    let alive = true;
    void api
      .getDetails(id)
      .then((d) => alive && setDetails(d))
      .catch((e) => alive && setError(errorMessage(e)));

    void api.subscribeConnections(id);
    const un = onConnections((c) => {
      if (c.id === id) setConns(c.connections);
    });

    return () => {
      alive = false;
      void api.unsubscribeConnections(id);
      void un.then((f) => f());
    };
  }, [id]);

  useEffect(() => {
    if (!live) return;
    speeds.current = [...speeds.current, live.current_bps].slice(-90);
    concurrency.current = [...concurrency.current, live.conns].slice(-90);
    bump((n) => n + 1);
  }, [live]);

  if (error) return <div className="drawer tabpane">{error}</div>;
  if (!details) return <div className="drawer tabpane note">Loading…</div>;

  const s = details.summary;

  return (
    <div className="drawer">
      <div className="tabs" role="tablist">
        {(["connections", "server", "stats", "links"] as Tab[]).map((t) => (
          <button
            key={t}
            role="tab"
            aria-selected={tab === t}
            className={tab === t ? "active" : ""}
            onClick={() => setTab(t)}
          >
            {t[0]!.toUpperCase() + t.slice(1)}
          </button>
        ))}
      </div>

      <div className="tabpane" role="tabpanel">
        {tab === "connections" && <ConnectionTable conns={conns} />}

        {tab === "server" && (
          <dl className="kv">
            <dt>Final address</dt>
            <dd className="mono">{details.final_url_redacted}</dd>
            <dt>Original address</dt>
            <dd className="mono">{details.original_url_redacted}</dd>
            <dt>Supports resuming</dt>
            <dd>{RANGE_SUPPORT[details.accept_ranges] ?? details.accept_ranges}</dd>
            <dt>Version tag</dt>
            <dd className="mono">{details.etag ?? "—"}</dd>
            <dt>Last modified</dt>
            <dd>{details.last_modified ?? "—"}</dd>
            <dt>Content type</dt>
            <dd>{details.content_type ?? "—"}</dd>
            <dt>HTTP version</dt>
            <dd>{details.http_version ?? "—"}</dd>
            <dt>Saved to</dt>
            <dd className="mono">{s.dest_dir}</dd>
            <dt>Partial file</dt>
            <dd className="mono">{details.part_path}</dd>
          </dl>
        )}

        {tab === "stats" && (
          <>
            <dl className="kv" style={{ marginBottom: 14 }}>
              <dt>Downloaded</dt>
              <dd>
                {bytes(s.bytes_done)}
                {s.total_size ? ` of ${bytes(s.total_size)}` : ""}
              </dd>
              <dt>Pieces on disk</dt>
              <dd>
                {details.completed_spans}{" "}
                {details.completed_spans === 1 ? "continuous piece" : "separate pieces"}
              </dd>
              <dt>Current speed</dt>
              <dd>{rate(live?.current_bps)}</dd>
              <dt>Peak speed</dt>
              <dd>{rate(live?.peak_bps)}</dd>
              <dt>Retries</dt>
              <dd>{details.retry_count}</dd>
              <dt>Link replaced</dt>
              <dd>{details.url_refresh_count} times</dd>
              <dt>Started</dt>
              <dd>{timestamp(s.created_at)}</dd>
              <dt>Finished</dt>
              <dd>{timestamp(s.completed_at)}</dd>
            </dl>
            <Sparkline values={speeds.current} label="Speed" format={rate} />
            {/* The governor made visible: this is the adaptive-concurrency claim, plotted. */}
            <Sparkline
              values={concurrency.current}
              label="Connections in use"
              height={30}
              format={(n) => String(Math.round(n))}
            />
          </>
        )}

        {tab === "links" && (
          <>
            <table className="data">
              <thead>
                <tr>
                  <th>#</th>
                  <th>Address</th>
                  <th>Source</th>
                  <th>Outcome</th>
                  <th>Kept</th>
                  <th>When</th>
                </tr>
              </thead>
              <tbody>
                {details.url_history.map((h) => (
                  <tr key={h.seq}>
                    <td>{h.seq}</td>
                    <td className="mono">{h.url_redacted}</td>
                    <td>{h.source.replace(/_/g, " ")}</td>
                    <td>{h.outcome.replace(/_/g, " ")}</td>
                    <td>{bytes(h.bytes_done_at_swap)}</td>
                    <td>{timestamp(h.added_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="note">
              Addresses are stored with their query values removed — a signed link is a
              credential, so keeping one in a history would be handing it out later.
            </p>
            {fullUrl ? (
              <p className="mono">{fullUrl}</p>
            ) : (
              <button
                className="btn subtle"
                onClick={async () => {
                  try {
                    setFullUrl(await api.revealFullUrl(id));
                  } catch (e) {
                    setError(errorMessage(e));
                  }
                }}
              >
                Copy full link…
              </button>
            )}
          </>
        )}
      </div>
    </div>
  );
}
