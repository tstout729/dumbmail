import { Card, Table, Td, Th } from "../../components/ui";
import { languageName, useLanguageReport } from "./api";

const pct = (n: number) => `${(n * 100).toFixed(1)}%`;

/** A campaign's results by the language each person was sent. Shown only when it went out in more than one. */
export function LanguageResults({ campaignId, sent }: { campaignId: string; sent: boolean }) {
  const report = useLanguageReport(campaignId, sent);
  const rows = (report.data?.data ?? []).filter((r) => r.language !== null);
  if (rows.length < 2) return null;
  return (
    <Card title="By language" padded={false}>
      <div className="overflow-x-auto">
        <Table>
          <thead>
            <tr>
              <Th>Language</Th>
              <Th className="text-right">Sent</Th>
              <Th className="text-right">Open rate</Th>
              <Th className="text-right">Click rate</Th>
              <Th className="text-right">Unsubscribed</Th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.language}>
                <Td className="font-medium text-zinc-900">
                  {languageName(r.language ?? "")} <span className="text-xs text-zinc-400">{r.language}</span>
                </Td>
                <Td className="text-right tabular-nums">{r.sent.toLocaleString()}</Td>
                <Td className="text-right tabular-nums">{pct(r.open_rate)}</Td>
                <Td className="text-right tabular-nums">{pct(r.click_rate)}</Td>
                <Td className="text-right tabular-nums">{r.unsubscribed.toLocaleString()}</Td>
              </tr>
            ))}
          </tbody>
        </Table>
      </div>
    </Card>
  );
}
