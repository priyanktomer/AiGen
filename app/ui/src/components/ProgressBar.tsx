import type { DownloadStatus } from "../api/bindings/DownloadStatus";
import { percent } from "../api/format";

/**
 * A download with no known size gets an indeterminate bar rather than an invented one. Showing
 * a percentage we cannot compute would be a straightforward lie about progress.
 */
export function ProgressBar({
  done,
  total,
  status,
}: {
  done: number;
  total: number | null;
  status: DownloadStatus;
}) {
  const pct = percent(done, total);
  const tone =
    status === "completed"
      ? "done"
      : status === "failed" || status === "needs_attention"
        ? "bad"
        : status === "active"
          ? ""
          : "stopped";

  if (pct === null && status === "active") {
    return (
      <div className="bar indeterminate">
        <i />
      </div>
    );
  }
  return (
    <div className={`bar ${tone}`}>
      <i style={{ width: `${pct ?? (status === "completed" ? 100 : 0)}%` }} />
    </div>
  );
}
