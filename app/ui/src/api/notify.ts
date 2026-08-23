// Desktop notifications.
//
// Raised from the UI rather than the shell so that the user's preference is consulted in one
// place — the window is always running while the app is, so there is no case where the shell
// could notify and the UI could not.
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";

let granted: boolean | null = null;

/**
 * Show a notification, asking for permission the first time.
 *
 * Silently does nothing if permission is refused. A download manager that nags for
 * notification permission it has already been denied is worse than one that stays quiet.
 */
export async function notify(title: string, body: string): Promise<void> {
  try {
    if (granted === null) {
      granted = await isPermissionGranted();
      if (!granted) granted = (await requestPermission()) === "granted";
    }
    if (granted) sendNotification({ title, body });
  } catch {
    // No notification service (common in containers and minimal desktops).
  }
}
