import { ROWS } from "@/lib/reports";

export default function Home() {
  return (
    <main>
      <h1>Acme Reports</h1>
      <p>Your signups and revenue by region. Viewing is free.</p>
      <table cellPadding={6} style={{ borderCollapse: "collapse" }}>
        <thead>
          <tr>
            <th align="left">Region</th>
            <th align="right">Signups</th>
            <th align="right">Revenue (sats)</th>
          </tr>
        </thead>
        <tbody>
          {ROWS.map((r) => (
            <tr key={r.region}>
              <td>{r.region}</td>
              <td align="right">{r.signups}</td>
              <td align="right">{r.revenueSats.toLocaleString()}</td>
            </tr>
          ))}
        </tbody>
      </table>

      <h2 style={{ marginTop: "2rem" }}>Pro export</h2>
      <p>
        Download the full dataset as CSV. This is a paid feature:{" "}
        <a href="/api/export">/api/export</a>.
      </p>
    </main>
  );
}
