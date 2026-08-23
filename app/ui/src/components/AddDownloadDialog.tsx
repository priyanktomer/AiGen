import { useState } from "react";

import { api, errorMessage } from "../api/commands";
import { bytes } from "../api/format";
import type { ProbePreview } from "../api/bindings/ProbePreview";

/**
 * Add a download.
 *
 * The clipboard is read only when the user clicks the button. Watching the clipboard in the
 * background is how download managers earn their reputation for being intrusive, and it is on
 * the roadmap as an opt-in, not here as a default.
 */
export function AddDownloadDialog({
  onClose,
  onAdded,
}: {
  onClose: () => void;
  onAdded: () => void;
}) {
  const [url, setUrl] = useState("");
  const [preview, setPreview] = useState<ProbePreview | null>(null);
  const [filename, setFilename] = useState("");
  const [conns, setConns] = useState<"auto" | number>("auto");
  const [startNow, setStartNow] = useState(true);
  const [probing, setProbing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const probe = async () => {
    setProbing(true);
    setError(null);
    try {
      const p = await api.probeUrl(url.trim());
      setPreview(p);
      setFilename(p.filename);
    } catch (e) {
      setError(errorMessage(e));
      setPreview(null);
    } finally {
      setProbing(false);
    }
  };

  const paste = async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (text) setUrl(text.trim());
    } catch {
      setError("Could not read the clipboard.");
    }
  };

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addDownload({
        url: url.trim(),
        dest_dir: null,
        filename: filename.trim() || null,
        category: null,
        max_conns: conns === "auto" ? null : conns,
        priority: "normal",
        start_now: startNow,
        expected_sha256: null,
      });
      onAdded();
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="scrim" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="dialog" role="dialog" aria-modal="true" aria-label="Add a download">
        <h2>Add a download</h2>
        <p className="sub">Paste a link. SwiftLoad will check it before anything is downloaded.</p>

        <div className="field">
          <label htmlFor="url">Link</label>
          <div className="row-inline">
            <input
              id="url"
              type="text"
              value={url}
              autoFocus
              placeholder="https://…"
              onChange={(e) => {
                setUrl(e.target.value);
                setPreview(null);
              }}
            />
            <button className="btn" onClick={() => void paste()} title="Paste from clipboard">
              Paste
            </button>
          </div>
        </div>

        <button className="btn" disabled={!url.trim() || probing} onClick={() => void probe()}>
          {probing ? "Checking…" : "Check link"}
        </button>

        {error && <div className="evidence no">{error}</div>}

        {preview && (
          <>
            <div className="evidence ok">
              <dl className="kv">
                <dt>File</dt>
                <dd>{preview.filename}</dd>
                <dt>Size</dt>
                <dd>{preview.total_size ? bytes(preview.total_size) : "Not stated by the server"}</dd>
                <dt>Resumable</dt>
                <dd>
                  {preview.resumable
                    ? "Yes — pausing and network loss are safe"
                    : "No — an interruption would mean starting again"}
                </dd>
                <dt>Split across connections</dt>
                <dd>{preview.segmentable ? "Yes" : "No — it will download as a single stream"}</dd>
                {preview.redirects > 0 && (
                  <>
                    <dt>Redirects</dt>
                    <dd>{preview.redirects}</dd>
                  </>
                )}
              </dl>
            </div>

            {/* Duplicate detection (§D.10.7): offer the partial rather than quietly starting a
                second copy of a file the user is already most of the way through. */}
            {preview.existing.length > 0 && (
              <div className="evidence ask">
                <strong>You already have an incomplete download for this file.</strong>
                {preview.existing.map((e) => (
                  <div className="note" key={e.id}>
                    {e.filename} — {bytes(e.bytes_done)}
                    {e.total_size ? ` of ${bytes(e.total_size)}` : ""} downloaded.{" "}
                    <button
                      className="btn subtle"
                      onClick={async () => {
                        await api.resume(e.id);
                        onAdded();
                        onClose();
                      }}
                    >
                      Resume that one instead
                    </button>
                  </div>
                ))}
              </div>
            )}

            <div className="field" style={{ marginTop: 14 }}>
              <label htmlFor="fname">Save as</label>
              <input
                id="fname"
                type="text"
                value={filename}
                onChange={(e) => setFilename(e.target.value)}
              />
            </div>

            <div className="field">
              <label htmlFor="conns">Connections</label>
              <select
                id="conns"
                value={String(conns)}
                onChange={(e) =>
                  setConns(e.target.value === "auto" ? "auto" : Number(e.target.value))
                }
              >
                <option value="auto">Auto — let SwiftLoad work it out</option>
                {[1, 2, 4, 8, 16, 32].map((n) => (
                  <option key={n} value={n}>
                    {n}
                  </option>
                ))}
              </select>
              <span className="hint">
                Auto measures what the server actually allows. A fixed number is only faster when
                you already know the answer.
              </span>
            </div>

            <label className="check">
              <input
                type="checkbox"
                checked={startNow}
                onChange={(e) => setStartNow(e.target.checked)}
              />
              Start now (otherwise it joins the queue)
            </label>
          </>
        )}

        <div className="buttons">
          <button className="btn subtle" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={!preview || busy} onClick={() => void add()}>
            {startNow ? "Download" : "Add to queue"}
          </button>
        </div>
      </div>
    </div>
  );
}
