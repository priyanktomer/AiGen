import type { ConnectionSnapshot } from "../api/bindings/ConnectionSnapshot";
import { bytes, rate } from "../api/format";

/**
 * Live per-connection state.
 *
 * This is the single most convincing view in the app — it is where "segmented download" stops
 * being a claim and becomes something you can watch. It is also the biggest payload the engine
 * produces, which is why it is only streamed while this drawer is open.
 */
export function ConnectionTable({ conns }: { conns: ConnectionSnapshot[] }) {
  if (conns.length === 0) {
    return <p className="note">No connections open. The download is not running right now.</p>;
  }
  return (
    <table className="data">
      <thead>
        <tr>
          <th>#</th>
          <th>Range</th>
          <th>Downloaded</th>
          <th>Speed</th>
          <th>Retries</th>
          <th>State</th>
        </tr>
      </thead>
      <tbody>
        {conns.map((c) => (
          <tr key={c.id}>
            <td>{c.id}</td>
            <td className="mono">
              {c.claim_start !== null && c.claim_end !== null
                ? `${bytes(c.claim_start)} – ${bytes(c.claim_end)}`
                : "—"}
            </td>
            <td>{bytes(c.bytes)}</td>
            <td>{rate(c.bps)}</td>
            <td>{c.retries}</td>
            <td>{c.state.replace(/_/g, " ")}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
