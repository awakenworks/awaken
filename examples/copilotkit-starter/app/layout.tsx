import "./globals.css";
import "@copilotkit/react-ui/styles.css";
import type { Metadata } from "next";

export const metadata: Metadata = {
  title: "Awaken CopilotKit Starter",
  icons: {
    icon: "/favicon.svg",
  },
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
