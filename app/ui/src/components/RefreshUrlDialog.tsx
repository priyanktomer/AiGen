import { useState } from "react";

import { api, errorMessage } from "../api/commands";
import { bytes } from "../api/format";
import type { RefreshReport } from "../api/bindings/RefreshReport";
import type { Row } from "../store/useDownloads";

/**
 * The §D.10 refresh flow.
 *
 * Two steps on purpose. `validate` mutates nothing, so the evidence is on screen before the
 * user commits to anything; `commit` re-checks server-side and can still refuse. The words
 * ETag, 206, range and byte offset appear nowhere — the user is being asked "is this the same
 * file?", and every one of those terms answers a question they did not ask.
 */
export function RefreshUrlDialog({
  row,
  onClose,
  onDone,
}: {
  row: Row;
  onClose: () => void;
  onDone: () => void;
}) {
  const [url, setUrl] = useState("");
  const [checking, setChecking] = useState(false);
  const [report, setReport] = useState<RefreshReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const validate = async () => {
    setChecking(true);
    setError(null);
    setReport(null);
    try {
      setReport(await api.validateReplacementUrl(row.id, url.trim()));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setChecking(false);
    }
  };

  const commit = async (acceptConfirm: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.commitReplacementUrl(row.id, url.trim(), acceptConfirm);
      onDone();
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="scrim" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="dialog" role="dialog" aria-modal="true" aria-label="Use a new URL">
        <h2>Use a new link for {row.filename}</h2>
        <p className="sub">
          {bytes(row.bytes_done)} already downloaded. Paste a fresh link to the same file and
          SwiftLoad will carry on from where it stopped.
        </p>

        <div className="field">
          <label htmlFor="newurl">New link</label>
          <input
            id="newurl"
            type="text"
            value={url}
            autoFocus
            placeholder="https://…"
            onChange={(e) => {
              setUrl(e.target.value);
              setReport(null);
            }}
          />
        </div>

        <button className="btn" disabled={!url.trim() || checking} onClick={() => void validate()}>
          {checking ? "Checking that this link is the same file…" : "Validate URL"}
        </button>

        {error && <div className="evidence no">{error}</div>}

        {report && <ResultCard report={report} busy={busy} onCommit={commit} onClose={onClose} />}

        {!report && (
          <div className="buttons">
            <button className="btn subtle" onClick={onClose}>
              Cancel
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

function ResultCard({
  report,
  busy,
  onCommit,
  onClose,
}: {
  report: RefreshReport;
  busy: boolean;
  onCommit: (acceptConfirm: boolean) => Promise<void>;
  onClose: () => void;
}) {
  const kept = bytes(report.preserved_bytes);
  const cost =
    report.windows_checked > 0
      ? `Checked ${report.windows_checked} sample section${report.windows_checked === 1 ? "" : "s"} — ${bytes(report.bytes_verified)} of traffic.`
      : null;

  switch (report.outcome) {
    case "verified":
      return (
        <div className="evidence ok">
          <strong>Same file confirmed.</strong>
          <div className="note">
            Resuming from {kept}. {cost}
          </div>
          <div className="buttons">
            <button className="btn subtle" onClick={onClose}>
              Not now
            </button>
            <button className="btn primary" disabled={busy} onClick={() => void onCommit(false)}>
              Resume from {kept}
            </button>
          </div>
        </div>
      );

    case "confirm":
      return (
        <div className="evidence ask">
          <strong>This looks like the same file.</strong>
          <div className="note">
            {report.message} {cost}
          </div>
          <div className="buttons">
            <button className="btn subtle" onClick={onClose}>
              Cancel
            </button>
            <button className="btn" disabled={busy} onClick={onClose}>
              Start over
            </button>
            <button className="btn primary" disabled={busy} onClick={() => void onCommit(true)}>
              Resume from {kept}
            </button>
          </div>
        </div>
      );

    case "restart_only":
      return (
        <div className="evidence ask">
          <strong>This link doesn&apos;t support resuming.</strong>
          <div className="note">
            Using it means downloading{" "}
            {report.total_size ? bytes(report.total_size) : "the whole file"} again. Your{" "}
            {kept} is kept either way — nothing here deletes it.
          </div>
          <div className="buttons">
            <button className="btn primary" onClick={onClose}>
              Keep waiting for a resumable link
            </button>
          </div>
        </div>
      );

    // A rejection has no "resume anyway", and offering one would be the single fastest way to
    // corrupt a large file.
    default:
      return (
        <div className="evidence no">
          <strong>That link doesn&apos;t match this download.</strong>
          <div className="note">{report.message}</div>
          <div className="buttons">
            <button className="btn subtle" onClick={onClose}>
              Cancel
            </button>
          </div>
        </div>
      );
  }
}
