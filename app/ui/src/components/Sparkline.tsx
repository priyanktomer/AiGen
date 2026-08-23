/**
 * A tiny inline chart. Two series matter here: throughput, and the connection count the
 * governor settled on — the second is what makes adaptive concurrency visible rather than
 * merely claimed.
 */
export function Sparkline({
  values,
  height = 40,
  label,
  format,
}: {
  values: number[];
  height?: number;
  label: string;
  format: (n: number) => string;
}) {
  if (values.length < 2) {
    return <p className="note">{label}: collecting…</p>;
  }
  const max = Math.max(...values, 1);
  const w = 240;
  const step = w / (values.length - 1);
  const points = values
    .map((v, i) => `${(i * step).toFixed(1)},${(height - (v / max) * height).toFixed(1)}`)
    .join(" ");

  return (
    <div style={{ marginBottom: 12 }}>
      <div className="note" style={{ marginTop: 0 }}>
        {label} · peak {format(max)}
      </div>
      <svg
        width="100%"
        viewBox={`0 0 ${w} ${height}`}
        preserveAspectRatio="none"
        style={{ display: "block", height }}
        role="img"
        aria-label={`${label}, peak ${format(max)}`}
      >
        <polyline
          points={points}
          fill="none"
          stroke="var(--accent)"
          strokeWidth="1.6"
          vectorEffect="non-scaling-stroke"
        />
      </svg>
    </div>
  );
}
