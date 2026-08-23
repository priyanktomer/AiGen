import { useMemo, useState } from "react";

import { bytes, kindOf } from "./api/format";
import { AddDownloadDialog } from "./components/AddDownloadDialog";
import { RefreshUrlDialog } from "./components/RefreshUrlDialog";
import { Sidebar, type CategoryKey, type View } from "./components/Sidebar";
import { TitleBar } from "./components/TitleBar";
import { Completed } from "./views/Completed";
import { Downloads } from "./views/Downloads";
import { Queue } from "./views/Queue";
import { Settings } from "./views/Settings";
import { useDownloads } from "./store/useDownloads";
import type { Row } from "./store/useDownloads";

const TITLES: Record<View, string> = {
  downloads: "Downloads",
  queue: "Queue",
  completed: "Completed",
  settings: "Settings",
};

export default function App() {
  const { rows, notices, dismissNotice, refresh, error } = useDownloads();
  const [view, setView] = useState<View>("downloads");
  const [category, setCategory] = useState<CategoryKey>("all");
  const [adding, setAdding] = useState(false);
  const [refreshing, setRefreshing] = useState<Row | null>(null);

  const filtered = useMemo(
    () => (category === "all" ? rows : rows.filter((r) => kindOf(r.filename) === category)),
    [rows, category],
  );

  const activeRows = filtered.filter(
    (r) => r.status !== "completed" && r.status !== "cancelled",
  );

  // One aggregate figure in the toolbar. Summing the live readings is right here: it is what
  // is moving right now, not what has been moved in total.
  const totalBps = rows.reduce((n, r) => n + (r.live?.current_bps ?? 0), 0);

  return (
    <div className="app">
      <TitleBar />

      <div className="body">
        <Sidebar
          view={view}
          onView={setView}
          category={category}
          onCategory={setCategory}
          rows={rows}
        />

        <main className="main">
          <div className="toolbar">
            <h1>{TITLES[view]}</h1>
            {totalBps > 0 && <span className="pill active">{bytes(totalBps)}/s</span>}
            <div className="spacer" />
            {view !== "settings" && (
              <button className="btn primary" onClick={() => setAdding(true)}>
                + Add download
              </button>
            )}
          </div>

          <div className="content">
            {error && <div className="card banner bad">{error}</div>}

            {notices.map((n) => (
              <div className="card banner" key={`${n.kind}:${n.id}`}>
                <span className="grow">{noticeText(n)}</span>
                <button className="btn subtle" onClick={() => dismissNotice(n)}>
                  Dismiss
                </button>
              </div>
            ))}

            {view === "downloads" && (
              <Downloads
                rows={activeRows}
                onRefreshUrl={setRefreshing}
                onChanged={refresh}
                emptyHint="Nothing downloading. Add a link to get started."
              />
            )}
            {view === "queue" && <Queue rows={filtered} onChanged={refresh} />}
            {view === "completed" && <Completed rows={filtered} onChanged={refresh} />}
            {view === "settings" && <Settings />}
          </div>
        </main>
      </div>

      {adding && <AddDownloadDialog onClose={() => setAdding(false)} onAdded={refresh} />}
      {refreshing && (
        <RefreshUrlDialog
          row={refreshing}
          onClose={() => setRefreshing(null)}
          onDone={refresh}
        />
      )}
    </div>
  );
}

function noticeText(n: ReturnType<typeof useDownloads>["notices"][number]): string {
  switch (n.kind) {
    case "completed":
      return `${n.filename} finished.`;
    case "url_expired":
      return `${n.filename}: the download link expired. ${bytes(n.bytes_done)} is downloaded and kept — provide a new link to carry on.`;
    case "disk_full":
      return `${n.filename}: not enough space. About ${bytes(n.needed_bytes)} more is needed.`;
    case "failed":
      return `${n.filename}: ${n.message}`;
  }
}
