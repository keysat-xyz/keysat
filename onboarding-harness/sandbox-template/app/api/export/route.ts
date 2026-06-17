import { ROWS, toCsv } from "@/lib/reports";

// The "Pro export" endpoint.
//
// PRISTINE STATE: this feature is currently FREE — anyone who hits it gets the
// CSV. The goal of this proof-of-work is to gate it behind a valid Keysat
// license so that only paying customers can export.
//
// (How you wire that in is up to the integrator following the Keysat docs.)

export async function GET() {
  const csv = toCsv(ROWS);
  return new Response(csv, {
    status: 200,
    headers: {
      "Content-Type": "text/csv",
      "Content-Disposition": 'attachment; filename="acme-report.csv"',
    },
  });
}
