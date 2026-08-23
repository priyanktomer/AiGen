import { useEffect, useState } from "react";

import { api, errorMessage } from "../api/commands";
import type { UiSettings } from "../api/bindings/UiSettings";

/**
 * Settings, grouped by what they protect rather than by which struct they came from, and each
 * group says why it exists. A number field with no explanation is a number nobody will ever
 * touch with any confidence.
 */
export function Settings() {
  const [s, setS] = useState<UiSettings | null>(null);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void api
      .getSettings()
      .then(setS)
      .catch((e) => setError(errorMessage(e)));
  }, []);

  if (error && !s) return <div className="empty">{error}</div>;
  if (!s) return <div className="empty">Loading…</div>;

  const set = <K extends keyof UiSettings>(k: K, v: UiSettings[K]) => {
    setS({ ...s, [k]: v });
    setSaved(false);
  };

  const save = async () => {
    try {
      await api.setSettings(s);
      setSaved(true);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const num = (
    label: string,
    key: keyof UiSettings,
    hint?: string,
    min = 1,
    max = 64,
  ) => (
    <div className="field">
      <label htmlFor={String(key)}>{label}</label>
      <input
        id={String(key)}
        type="number"
        min={min}
        max={max}
        value={Number(s[key])}
        onChange={(e) => set(key, Number(e.target.value) as never)}
      />
      {hint && <span className="hint">{hint}</span>}
    </div>
  );

  const check = (label: string, key: keyof UiSettings, hint?: string) => (
    <div className="field">
      <label className="check" style={{ marginBottom: 0 }}>
        <input
          type="checkbox"
          checked={Boolean(s[key])}
          onChange={(e) => set(key, e.target.checked as never)}
        />
        {label}
      </label>
      {hint && <span className="hint">{hint}</span>}
    </div>
  );

  return (
    <div style={{ maxWidth: 720 }}>
      <div className="settings-group">
        <h3>Where files go</h3>
        <div className="field">
          <label htmlFor="dir">Download folder</label>
          <div className="row-inline">
            <input
              id="dir"
              type="text"
              value={s.download_dir}
              onChange={(e) => set("download_dir", e.target.value)}
            />
            <button
              className="btn"
              onClick={async () => {
                const picked = await api.chooseFolder();
                if (picked) set("download_dir", picked);
              }}
            >
              Browse…
            </button>
          </div>
        </div>
      </div>

      <div className="settings-group">
        <h3>How much at once</h3>
        <p className="why">
          These three limits protect different things and none substitutes for another: your
          attention, the server&apos;s patience, and your own link.
        </p>
        <div className="grid2">
          {num("Downloads at once", "max_concurrent_downloads", "Beyond this, downloads queue.")}
          {num(
            "Connections per download",
            "max_conns_per_download",
            "A ceiling, not a target — the governor stays under it.",
          )}
          {num(
            "Connections per server",
            "max_conns_per_host",
            "Good manners, and most servers cap anyway.",
          )}
          {num(
            "Connections in total",
            "max_total_conns",
            "Stops three downloads at sixteen each from becoming forty-eight sockets.",
            1,
            128,
          )}
        </div>
        {check(
          "Work out the connection count automatically",
          "adaptive_concurrency",
          "Measures whether more connections actually help on this server. Turning it off pins every download to the per-download limit above, which is faster only when you already know the answer.",
        )}
      </div>

      <div className="settings-group">
        <h3>When things go wrong</h3>
        <div className="grid2">
          {num("Retries per segment", "max_retries_per_segment", undefined, 0, 32)}
          {num("Connect timeout (seconds)", "connect_timeout_secs", undefined, 1, 120)}
          {num(
            "Idle timeout (seconds)",
            "read_idle_timeout_secs",
            "Time between reads, not a deadline for the whole file.",
            1,
            600,
          )}
          {num("Stall timeout (seconds)", "stall_timeout_secs", undefined, 1, 600)}
        </div>
        {check(
          "Re-fetch a margin after an unclean shutdown",
          "paranoid_recovery",
          "Costs about a megabyte per piece after a crash, and covers drives that report a write as durable before it is.",
        )}
      </div>

      <div className="settings-group">
        <h3>Safety and integrity</h3>
        {check(
          "Mark downloads as coming from the internet",
          "apply_motw",
          "Windows and its antivirus then treat the file exactly as they would a browser download. Turning this off makes files look more trusted than they are.",
        )}
        {check(
          "Hash each file when it finishes",
          "hash_on_complete",
          "Lets integrity be reported as verified rather than assumed.",
        )}
        {check(
          "Refuse redirects that downgrade to plain HTTP",
          "block_insecure_redirect",
          "A link that starts secure and quietly stops being secure is worth refusing.",
        )}
        <div className="field">
          <label htmlFor="collision">If a file with that name already exists</label>
          <select
            id="collision"
            value={s.collision_policy}
            onChange={(e) => set("collision_policy", e.target.value as UiSettings["collision_policy"])}
          >
            <option value="rename">Keep both — add a number, like Explorer does</option>
            <option value="overwrite">Overwrite it</option>
            <option value="ask">Ask me</option>
          </select>
        </div>
      </div>

      <div className="row-inline">
        <button className="btn primary" onClick={() => void save()}>
          Save settings
        </button>
        {saved && <span className="note" style={{ marginTop: 0 }}>Saved.</span>}
        {error && <span className="note" style={{ marginTop: 0, color: "var(--danger)" }}>{error}</span>}
      </div>
      <p className="note">
        Changed limits apply to downloads that start from now on. Lowering a limit never stops
        something already running — that would destroy work to satisfy a preference.
      </p>
    </div>
  );
}
