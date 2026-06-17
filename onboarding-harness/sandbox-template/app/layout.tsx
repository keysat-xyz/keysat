import type { ReactNode } from "react";

export const metadata = {
  title: "Acme Reports",
  description: "A tiny analytics tool with a paid Pro export.",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <body style={{ fontFamily: "system-ui, sans-serif", maxWidth: 640, margin: "3rem auto", padding: "0 1rem" }}>
        {children}
      </body>
    </html>
  );
}
