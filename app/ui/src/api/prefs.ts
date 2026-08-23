// Per-machine interface preferences.
//
// Kept in localStorage rather than in the engine's settings, because none of them changes what
// a download *does* — they change what this window looks like and whether it interrupts you.
// Storing them alongside connection limits would imply they travel with the download history,
// and they should not.
//
// The one exception is start-with-Windows, which is not here: it is a real OS registration, so
// it is read from and written to the OS rather than mirrored in a preference that could drift.

export type Theme = "system" | "light" | "dark";

export interface Prefs {
  theme: Theme;
  notifyOnComplete: boolean;
  notifyOnProblem: boolean;
}

const KEY = "swiftload.prefs";

const DEFAULTS: Prefs = {
  theme: "system",
  notifyOnComplete: true,
  // A failure or an expired link is something the user has to act on, so it is worth an
  // interruption in a way that a routine completion is not — but both stay switchable.
  notifyOnProblem: true,
};

export function loadPrefs(): Prefs {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return { ...DEFAULTS };
    return { ...DEFAULTS, ...(JSON.parse(raw) as Partial<Prefs>) };
  } catch {
    // A corrupt or unavailable store must not stop the app from opening.
    return { ...DEFAULTS };
  }
}

export function savePrefs(p: Prefs): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(p));
  } catch {
    // Private-mode browsers and locked-down profiles refuse writes; the app still works.
  }
}

/** Apply the theme choice. "system" removes the attribute so the media query decides. */
export function applyTheme(theme: Theme): void {
  const root = document.documentElement;
  if (theme === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", theme);
}
