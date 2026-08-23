import type { Row } from "../store/useDownloads";

export type View = "downloads" | "queue" | "completed" | "settings";

const CATEGORIES = [
  { key: "all", label: "All files", icon: "▦" },
  { key: "video", label: "Video", icon: "▶" },
  { key: "audio", label: "Audio", icon: "♪" },
  { key: "archive", label: "Archives", icon: "▤" },
  { key: "app", label: "Programs", icon: "◈" },
  { key: "doc", label: "Documents", icon: "▧" },
] as const;

export type CategoryKey = (typeof CATEGORIES)[number]["key"];

export function Sidebar({
  view,
  onView,
  category,
  onCategory,
  rows,
}: {
  view: View;
  onView: (v: View) => void;
  category: CategoryKey;
  onCategory: (c: CategoryKey) => void;
  rows: Row[];
}) {
  const active = rows.filter(
    (r) => r.status !== "completed" && r.status !== "cancelled",
  ).length;
  const queued = rows.filter((r) => r.status === "queued").length;
  const done = rows.filter((r) => r.status === "completed").length;

  const item = (key: View, label: string, icon: string, count?: number) => (
    <button
      key={key}
      className={view === key ? "active" : ""}
      onClick={() => onView(key)}
    >
      <span aria-hidden>{icon}</span>
      {label}
      {count !== undefined && count > 0 && <span className="count">{count}</span>}
    </button>
  );

  return (
    <nav className="sidebar">
      <div className="nav">
        {item("downloads", "Downloads", "⤓", active)}
        {item("queue", "Queue", "≡", queued)}
        {item("completed", "Completed", "✓", done)}
        {item("settings", "Settings", "⚙")}
      </div>

      {view !== "settings" && (
        <div>
          <div className="side-label">Filter</div>
          <div className="nav">
            {CATEGORIES.map((c) => (
              <button
                key={c.key}
                className={category === c.key ? "active" : ""}
                onClick={() => onCategory(c.key)}
              >
                <span aria-hidden>{c.icon}</span>
                {c.label}
              </button>
            ))}
          </div>
        </div>
      )}
    </nav>
  );
}
