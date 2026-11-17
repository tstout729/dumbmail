import { useState } from "react";
import { useNavigate } from "react-router";
import { Plus, TicketPercent } from "lucide-react";
import { useSession } from "../../api";
import { Button, EmptyRow, EmptyState, InlineError, Modal, Notice, Page, PageHeader, RowLink, SkeletonRows, Table, Td, Th } from "../../components/ui";
import { formatNumber, pluralize } from "../../lib/format";
import { useCoupons, useCreateCoupon } from "./api";
import { CouponSettings, draftFrom, draftProblem, expiryText, inputFrom, StockBadge } from "./parts";
import type { Coupon } from "./types";

export function CouponsPage() {
  const { account } = useSession();
  const coupons = useCoupons();
  const [creating, setCreating] = useState(false);
  const rows = coupons.data?.data ?? [];
  const empty = rows.filter((c) => c.alerts.out_of_codes);
  const low = rows.filter((c) => !c.alerts.out_of_codes && c.alerts.low_stock);
  const waiting = empty.reduce((n, c) => n + c.alerts.waiting.length, 0);

  return (
    <Page>
      <PageHeader
        title="Coupons"
        description="Pools of single-use codes. Everyone gets their own code in emails, texts and sign-up forms."
        actions={
          <Button variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setCreating(true)}>
            New coupon
          </Button>
        }
      />
      {empty.length ? (
        <div className="mb-4">
          <Notice tone="red" title={`${empty.map((c) => c.name).join(", ")} ${empty.length === 1 ? "is" : "are"} out of codes`}>
            {waiting ? `${pluralize(waiting, "campaign or flow", "campaigns and flows")} ${waiting === 1 ? "is" : "are"} waiting. ` : ""}
            Messages that use {empty.length === 1 ? "it" : "them"} wait until you add codes, and sign-up forms show no code.
          </Notice>
        </div>
      ) : null}
      {low.length ? (
        <div className="mb-4">
          <Notice tone="amber" title={`${low.map((c) => c.name).join(", ")} ${low.length === 1 ? "is" : "are"} running low`}>
            Add codes before {low.length === 1 ? "it runs" : "they run"} out.
          </Notice>
        </div>
      ) : null}
      <Table>
        <thead>
          <tr>
            <Th>Coupon</Th>
            <Th>Stock</Th>
            <Th className="hidden sm:table-cell">Given</Th>
            <Th className="hidden sm:table-cell">Redeemed</Th>
            <Th className="hidden md:table-cell">Expiry</Th>
          </tr>
        </thead>
        <tbody>
          {coupons.isPending ? (
            <SkeletonRows cols={5} rows={4} />
          ) : coupons.error ? (
            <EmptyRow cols={5}>
              <EmptyState title="Couldn't load coupons" description={coupons.error.message} />
            </EmptyRow>
          ) : rows.length === 0 ? (
            <EmptyRow cols={5}>
              <EmptyState
                icon={<TicketPercent className="size-5" />}
                title="No coupons yet"
                description="A coupon gives each person their own single-use code, so codes can't be shared and every use is tied to someone."
                action={
                  <Button variant="primary" onClick={() => setCreating(true)}>
                    New coupon
                  </Button>
                }
              />
            </EmptyRow>
          ) : (
            rows.map((c) => <CouponRow key={c.id} c={c} timeZone={account.timezone} />)
          )}
        </tbody>
      </Table>
      {creating ? <CreateCouponDialog onClose={() => setCreating(false)} /> : null}
    </Page>
  );
}

function CouponRow({ c, timeZone }: { c: Coupon; timeZone: string }) {
  return (
    <RowLink to={`/content/coupons/${c.id}`}>
      <Td>
        <div className="min-w-0 max-w-[11rem] sm:max-w-xs">
          <div className="truncate font-mono font-medium text-zinc-900">{c.name}</div>
          <div className="truncate text-xs text-zinc-500">{c.description || (c.source === "generated" ? "Codes made by DumbMail" : "Codes from your store")}</div>
        </div>
      </Td>
      <Td>
        <StockBadge c={c} />
      </Td>
      <Td className="hidden tabular-nums text-zinc-700 sm:table-cell">{formatNumber(c.stock.assigned + c.stock.redeemed)}</Td>
      <Td className="hidden tabular-nums text-zinc-700 sm:table-cell">{formatNumber(c.stock.redeemed)}</Td>
      <Td className="hidden whitespace-nowrap text-zinc-600 md:table-cell">{expiryText(c, timeZone)}</Td>
    </RowLink>
  );
}

function CreateCouponDialog({ onClose }: { onClose: () => void }) {
  const { account } = useSession();
  const navigate = useNavigate();
  const create = useCreateCoupon();
  const [draft, setDraft] = useState(() => draftFrom(null, account.timezone));
  const [tried, setTried] = useState(false);
  const problem = draftProblem(draft, true);
  const submit = () => {
    setTried(true);
    if (problem) return;
    create.mutate(inputFrom(draft, account.timezone, true), { onSuccess: (c) => navigate(`/content/coupons/${c.id}`) });
  };
  return (
    <Modal
      title="New coupon"
      description="Name it, then add codes on the next screen."
      onClose={onClose}
      size="lg"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" loading={create.isPending} onClick={submit}>
            Create coupon
          </Button>
        </>
      }
    >
      <CouponSettings draft={draft} onChange={setDraft} creating />
      <div className="mt-3">
        <InlineError message={tried ? problem : null} />
      </div>
    </Modal>
  );
}
