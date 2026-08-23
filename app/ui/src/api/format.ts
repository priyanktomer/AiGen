// Presentation helpers.
//
// These decide what the user reads, so the rules are here rather than scattered through
// components: a size shown two different ways in two places reads as a bug.

/** Bytes as a human size. Binary units, because that is what a file manager shows. */
export function bytes(n: number | null | undefined): string {
  if (n === null || n === undefined) return "—";
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

export function rate(bps: number | null | undefined): string {
  if (!bps) return "—";
  return `${bytes(bps)}/s`;
}

/**
 * Seconds as a duration.
 *
 * Returns "—" rather than a guess when there is nothing to estimate from. A confidently wrong
 * "2 seconds left" that sits there for a minute is worse than admitting we do not know.
 */
export function duration(secs: number | null | undefined): string {
  if (secs === null || secs === undefined) return "—";
  if (secs < 60) return `${Math.round(secs)}s`;
  const m = Math.floor(secs / 60);
  const s = Math.round(secs % 60);
  if (m < 60) return `${m}m ${s.toString().padStart(2, "0")}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${(m % 60).toString().padStart(2, "0")}m`;
}

export function percent(done: number, total: number | null | undefined): number | null {
  if (!total || total <= 0) return null;
  return Math.min(100, (done / total) * 100);
}

export function timestamp(secs: number | null | undefined): string {
  if (!secs) return "—";
  return new Date(secs * 1000).toLocaleString();
}

/** A rough file-type label from the name, for the row icon. */
export function kindOf(filename: string): string {
  const ext = filename.split(".").pop()?.toLowerCase() ?? "";
  if (["mp4", "mkv", "avi", "mov", "webm", "m4v"].includes(ext)) return "video";
  if (["mp3", "flac", "wav", "aac", "ogg", "m4a"].includes(ext)) return "audio";
  if (["zip", "rar", "7z", "tar", "gz", "xz", "zst"].includes(ext)) return "archive";
  if (["exe", "msi", "dmg", "pkg", "appimage", "deb", "rpm"].includes(ext)) return "app";
  if (["pdf", "doc", "docx", "txt", "epub", "xlsx", "pptx"].includes(ext)) return "doc";
  if (["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"].includes(ext)) return "image";
  if (["iso", "img"].includes(ext)) return "disk";
  return "file";
}
