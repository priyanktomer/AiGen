// Engine events, as delivered by the shell's EventPump.
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type { ConnectionsSnapshot } from "./bindings/ConnectionsSnapshot";
import type { Notice } from "./bindings/Notice";
import type { ProgressSnapshot } from "./bindings/ProgressSnapshot";
import type { StateChanged } from "./bindings/StateChanged";

// One event per tick carrying every active download, not one event per download.
export const onProgress = (f: (p: ProgressSnapshot[]) => void): Promise<UnlistenFn> =>
  listen<ProgressSnapshot[]>("progress-tick", (e) => f(e.payload));

// Coalesced by the pump: a burst of transitions arrives as one batch.
export const onStateChanged = (f: (s: StateChanged[]) => void): Promise<UnlistenFn> =>
  listen<StateChanged[]>("state-changed", (e) => f(e.payload));

// Only arrives while a Details drawer has subscribed.
export const onConnections = (f: (c: ConnectionsSnapshot) => void): Promise<UnlistenFn> =>
  listen<ConnectionsSnapshot>("connections", (e) => f(e.payload));

// Needs the user.
export const onNotice = (f: (n: Notice) => void): Promise<UnlistenFn> =>
  listen<Notice>("notice", (e) => f(e.payload));
