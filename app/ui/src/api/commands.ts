// Typed wrappers over the Tauri command surface.
//
// Every argument and return type here is generated from the Rust definitions by ts-rs, so a
// renamed field is a build failure rather than an `undefined` discovered by a user.
import { invoke } from "@tauri-apps/api/core";

import type { AddRequest } from "./bindings/AddRequest";
import type { DownloadDetails } from "./bindings/DownloadDetails";
import type { DownloadStatus } from "./bindings/DownloadStatus";
import type { DownloadSummary } from "./bindings/DownloadSummary";
import type { Priority } from "./bindings/Priority";
import type { ProbePreview } from "./bindings/ProbePreview";
import type { RefreshReport } from "./bindings/RefreshReport";
import type { UiSettings } from "./bindings/UiSettings";
import type { UrlHistoryRow } from "./bindings/UrlHistoryRow";

export const api = {
  probeUrl: (url: string) => invoke<ProbePreview>("probe_url", { url }),
  addDownload: (request: AddRequest) => invoke<string>("add_download", { request }),

  pause: (id: string) => invoke<void>("pause", { id }),
  resume: (id: string) => invoke<void>("resume", { id }),
  retry: (id: string) => invoke<void>("retry", { id }),
  cancel: (id: string) => invoke<void>("cancel", { id }),
  remove: (id: string, deleteFile: boolean) =>
    invoke<void>("remove", { id, deleteFile }),

  setPriority: (id: string, priority: Priority) =>
    invoke<boolean>("set_priority", { id, priority }),
  startNow: (id: string) => invoke<boolean>("start_now", { id }),

  listDownloads: (status?: DownloadStatus) =>
    invoke<DownloadSummary[]>("list_downloads", { status: status ?? null }),
  getDetails: (id: string) => invoke<DownloadDetails>("get_details", { id }),
  getUrlHistory: (id: string) => invoke<UrlHistoryRow[]>("get_url_history", { id }),
  revealFullUrl: (id: string) => invoke<string>("reveal_full_url", { id }),

  subscribeConnections: (id: string) => invoke<void>("subscribe_connections", { id }),
  unsubscribeConnections: (id: string) => invoke<void>("unsubscribe_connections", { id }),

  getSettings: () => invoke<UiSettings>("get_settings"),
  setSettings: (settings: UiSettings) => invoke<void>("set_settings", { settings }),

  validateReplacementUrl: (id: string, url: string) =>
    invoke<RefreshReport>("validate_replacement_url", { id, url }),
  commitReplacementUrl: (id: string, url: string, acceptConfirm: boolean) =>
    invoke<void>("commit_replacement_url", { id, url, acceptConfirm }),

  chooseFolder: () => invoke<string | null>("choose_folder"),
  revealInExplorer: (id: string) => invoke<void>("reveal_in_explorer", { id }),
  openFile: (id: string) => invoke<void>("open_file", { id }),
};

/// Errors cross the IPC boundary as the serialised `ManagerError`. Render the message; the
/// discriminant is there for the rare case where the UI needs to branch on the kind.
export function errorMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object") {
    const m = (e as { message?: unknown }).message;
    if (typeof m === "string") return m;
  }
  return "Something went wrong.";
}
