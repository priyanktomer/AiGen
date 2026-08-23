import type { DownloadStatus } from "../api/bindings/DownloadStatus";

// The engine's status names are not all suitable for a user to read.
const LABEL: Record<DownloadStatus, string> = {
  queued: "Queued",
  active: "Downloading",
  paused: "Paused",
  retrying: "Retrying",
  waiting_network: "Waiting for network",
  needs_attention: "Needs attention",
  completed: "Completed",
  failed: "Failed",
  cancelled: "Cancelled",
};

export function StatusPill({ status }: { status: DownloadStatus }) {
  return <span className={`pill ${status}`}>{LABEL[status]}</span>;
}
