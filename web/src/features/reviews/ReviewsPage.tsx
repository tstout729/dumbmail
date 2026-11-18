import { useSearchParams } from "react-router";
import { Page, PageHeader, Tabs } from "../../components/ui";
import { ImportTab } from "./ImportTab";
import { InstallTab } from "./InstallTab";
import { ProductsTab } from "./ProductsTab";
import { QueueTab } from "./QueueTab";
import { ReportTab } from "./ReportTab";
import { SettingsTab } from "./SettingsTab";
import { useReviews } from "./api";

type Tab = "queue" | "products" | "report" | "import" | "settings" | "install";
const tabs: Tab[] = ["queue", "products", "report", "import", "settings", "install"];

export function ReviewsPage() {
  const [params, setParams] = useSearchParams();
  const raw = params.get("tab");
  const tab: Tab = tabs.find((t) => t === raw) ?? "queue";
  const waiting = useReviews({ status: "pending", unconfirmed: false, limit: 1 });
  const setTab = (t: Tab) => {
    const next = new URLSearchParams();
    if (t !== "queue") next.set("tab", t);
    setParams(next, { replace: true });
  };
  return (
    <Page>
      <PageHeader title="Reviews" description="Ask customers for reviews after they buy or stay, check them, and show them on your site and in emails." />
      <Tabs
        value={tab}
        onChange={setTab}
        tabs={[
          { value: "queue", label: "Reviews", count: waiting.data?.total || undefined },
          { value: "products", label: "Products" },
          { value: "report", label: "Report" },
          { value: "import", label: "Import & export" },
          { value: "settings", label: "Settings" },
          { value: "install", label: "Add to your site" },
        ]}
      />
      {tab === "queue" ? <QueueTab /> : null}
      {tab === "products" ? <ProductsTab /> : null}
      {tab === "report" ? <ReportTab /> : null}
      {tab === "import" ? <ImportTab /> : null}
      {tab === "settings" ? <SettingsTab /> : null}
      {tab === "install" ? <InstallTab /> : null}
    </Page>
  );
}
