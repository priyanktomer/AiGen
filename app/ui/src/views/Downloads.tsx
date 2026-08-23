import { DownloadRow } from "../components/DownloadRow";
import type { Row } from "../store/useDownloads";

export function Downloads({
  rows,
  onRefreshUrl,
  onChanged,
  emptyHint,
}: {
  rows: Row[];
  onRefreshUrl: (row: Row) => void;
  onChanged: () => void;
  emptyHint: string;
}) {
  if (rows.length === 0) {
    return (
      <div className="empty">
        <div className="big" aria-hidden>
          ⤓
        </div>
        {emptyHint}
      </div>
    );
  }
  return (
    <>
      {rows.map((r) => (
        <DownloadRow key={r.id} row={r} onRefreshUrl={onRefreshUrl} onChanged={onChanged} />
      ))}
    </>
  );
}
