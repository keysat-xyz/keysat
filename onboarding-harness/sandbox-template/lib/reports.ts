// The "data" behind Acme Reports. The free tier lets you view it on screen;
// the paid "Pro export" feature lets you download it as CSV. That export is
// the feature we want to gate behind a Keysat license.

export type Row = { region: string; signups: number; revenueSats: number };

export const ROWS: Row[] = [
  { region: "North", signups: 412, revenueSats: 1_240_000 },
  { region: "South", signups: 318, revenueSats: 980_500 },
  { region: "East", signups: 521, revenueSats: 1_702_300 },
  { region: "West", signups: 274, revenueSats: 731_900 },
];

export function toCsv(rows: Row[]): string {
  const header = "region,signups,revenue_sats";
  const body = rows.map((r) => `${r.region},${r.signups},${r.revenueSats}`);
  return [header, ...body].join("\n") + "\n";
}
