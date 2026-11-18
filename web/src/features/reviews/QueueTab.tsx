import { useState } from "react";
import { Camera, Check, ChevronLeft, ChevronRight, Download, MessageSquare, Pin, Search, ShieldCheck, Star, Trash2, X } from "lucide-react";
import { useSession } from "../../api";
import { Badge, Button, Card, Checkbox, ConfirmDialog, EmptyState, Field, IconButton, InlineError, Input, Modal, Notice, Select, Skeleton, Textarea } from "../../components/ui";
import { formatDate, formatNumber, pluralize } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { exportReviews, useDeleteReview, useModerate, useRemovePhoto, useReply, useReviews } from "./api";
import { flagLabel, rejectReasonLabels, ReviewStatusBadge, Stars } from "./parts";
import type { ModerateAction, RejectReason, Review, ReviewQuery, ReviewStatus } from "./types";

const limit = 25;

type View = ReviewStatus | "unconfirmed" | "all";

function viewQuery(v: View): ReviewQuery {
  if (v === "all") return {};
  if (v === "unconfirmed") return { unconfirmed: true };
  return { status: v, unconfirmed: v === "pending" ? false : undefined };
}

export function QueueTab() {
  const { account } = useSession();
  const canManage = useCan("reviews.manage");
  const canDelete = useCan("catalog.delete");
  const canExport = useCan("export");
  const canManageExports = useCan("exports.manage");
  const canReadProfiles = useCan("profiles.read");
  const canExportPeople = canManageExports && canReadProfiles;
  const [view, setView] = useState<View>("pending");
  const [rating, setRating] = useState("");
  const [verified, setVerified] = useState("");
  const [photos, setPhotos] = useState("");
  const [product, setProduct] = useState("");
  const [since, setSince] = useState("");
  const [until, setUntil] = useState("");
  const [search, setSearch] = useState("");
  const q = useDebouncedValue(search.trim(), 250);
  const productQ = useDebouncedValue(product.trim(), 250);
  const filters: ReviewQuery = {
    ...viewQuery(view),
    rating: rating ? Number(rating) : undefined,
    verified: verified === "" ? undefined : verified === "yes",
    has_photos: photos === "" ? undefined : photos === "yes",
    product_id: productQ || undefined,
    since: since || undefined,
    until: until || undefined,
    q: q || undefined,
  };
  const key = JSON.stringify(filters);
  const [paging, setPaging] = useState({ key: "", offset: 0 });
  const offset = paging.key === key ? paging.offset : 0;
  const list = useReviews({ ...filters, limit, offset });
  const [picked, setPicked] = useState<{ key: string; ids: string[] }>({ key: "", ids: [] });
  const selected = picked.key === key ? picked.ids : [];
  const setSelected = (ids: string[]) => setPicked({ key, ids });
  const [bulk, setBulk] = useState<"publish" | "reject" | null>(null);
  const rows = list.data?.data ?? [];
  const counts = list.data?.counts ?? {};
  const total = list.data?.total ?? 0;

  const views: { value: View; label: string; count?: number }[] = [
    { value: "pending", label: "Waiting", count: counts.pending },
    { value: "published", label: "Published", count: counts.published },
    { value: "rejected", label: "Rejected", count: counts.rejected },
    { value: "removed", label: "Deleted by author", count: counts.removed },
    { value: "unconfirmed", label: "Email not confirmed", count: counts.awaiting_confirmation },
    { value: "all", label: "All" },
  ];

  return (
    <div className="space-y-4">
      {view === "pending" && (counts.pending ?? 0) > 0 ? (
        <Notice tone="blue" icon={<ShieldCheck />} title="Decide on what a review says, not its rating">
          {list.data?.note}
        </Notice>
      ) : null}
      <div className="flex flex-wrap gap-1.5" role="group" aria-label="Show reviews">
        {views.map((v) => (
          <button
            key={v.value}
            type="button"
            aria-pressed={view === v.value}
            onClick={() => setView(v.value)}
            className={`rounded-full px-3 py-1 text-sm font-medium ring-1 ring-inset transition-colors ${view === v.value ? "bg-zinc-900 text-white ring-zinc-900" : "bg-white text-zinc-700 ring-zinc-200 hover:bg-zinc-50"}`}
          >
            {v.label}
            {v.count ? <span className="ml-1.5 tabular-nums opacity-70">{formatNumber(v.count)}</span> : null}
          </button>
        ))}
      </div>
      <Card padded>
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <Field label="Search">
            <div className="relative">
              <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
              <Input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Words or names" className="w-full pl-8" />
            </div>
          </Field>
          <Field label="Product id">
            <Input value={product} onChange={(e) => setProduct(e.target.value)} placeholder="Any product" />
          </Field>
          <Field label="Rating">
            <Select value={rating} onChange={(e) => setRating(e.target.value)}>
              <option value="">Any</option>
              {[5, 4, 3, 2, 1].map((n) => (
                <option key={n} value={n}>
                  {pluralize(n, "star")}
                </option>
              ))}
            </Select>
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Verified">
              <Select value={verified} onChange={(e) => setVerified(e.target.value)}>
                <option value="">Any</option>
                <option value="yes">Verified</option>
                <option value="no">Not verified</option>
              </Select>
            </Field>
            <Field label="Photos">
              <Select value={photos} onChange={(e) => setPhotos(e.target.value)}>
                <option value="">Any</option>
                <option value="yes">With photos</option>
                <option value="no">No photos</option>
              </Select>
            </Field>
          </div>
          <Field label="From">
            <Input type="date" value={since} onChange={(e) => setSince(e.target.value)} />
          </Field>
          <Field label="To">
            <Input type="date" value={until} onChange={(e) => setUntil(e.target.value)} />
          </Field>
          <div className="flex items-end gap-2 sm:col-span-2">
            {canExport ? (
              <Button icon={<Download className="size-3.5" />} onClick={() => exportReviews(filters).catch((e: Error) => toast.error(e.message))}>
                Export CSV
              </Button>
            ) : null}
            {canExport && canExportPeople ? (
              <Button variant="ghost" icon={<Download className="size-3.5" />} onClick={() => exportReviews(filters, true).catch((e: Error) => toast.error(e.message))}>
                With reviewers' emails
              </Button>
            ) : null}
          </div>
        </div>
      </Card>
      {canManage && rows.length > 0 ? (
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <Checkbox
            label={selected.length ? `${formatNumber(selected.length)} selected` : "Select all on this page"}
            checked={selected.length > 0 && selected.length === rows.filter((r) => r.status !== "removed").length}
            onChange={(e) => setSelected(e.target.checked ? rows.filter((r) => r.status !== "removed").map((r) => r.id) : [])}
          />
          {selected.length ? (
            <>
              <Button size="sm" icon={<Check className="size-3.5" />} onClick={() => setBulk("publish")}>
                Publish
              </Button>
              <Button size="sm" icon={<X className="size-3.5" />} onClick={() => setBulk("reject")}>
                Reject
              </Button>
            </>
          ) : null}
        </div>
      ) : null}
      <div className="space-y-3">
        {list.isPending ? (
          [0, 1, 2].map((i) => <Skeleton key={i} className="h-28 w-full" />)
        ) : list.error ? (
          <Card>
            <EmptyState title="Couldn't load reviews" description={list.error.message} />
          </Card>
        ) : rows.length === 0 ? (
          <Card>
            <EmptyState
              icon={<Star className="size-5" />}
              title={view === "pending" ? "Nothing waiting" : "No reviews here"}
              description={view === "pending" ? "Reviews that need a look show up here. Set up the review flow under Settings to start asking customers." : "Try other filters."}
            />
          </Card>
        ) : (
          rows.map((r) => (
            <ReviewCard
              key={r.id}
              r={r}
              timeZone={account.timezone}
              canManage={canManage}
              canDelete={canDelete}
              selected={selected.includes(r.id)}
              onSelect={(on) => setSelected(on ? [...selected, r.id] : selected.filter((x) => x !== r.id))}
            />
          ))
        )}
      </div>
      {total > limit ? (
        <div className="flex items-center justify-between text-sm text-zinc-600">
          <span>
            {formatNumber(offset + 1)}–{formatNumber(Math.min(offset + limit, total))} of {formatNumber(total)}
          </span>
          <div className="flex gap-1">
            <IconButton label="Previous page" disabled={offset === 0} onClick={() => setPaging({ key, offset: Math.max(0, offset - limit) })}>
              <ChevronLeft className="size-4" />
            </IconButton>
            <IconButton label="Next page" disabled={offset + limit >= total} onClick={() => setPaging({ key, offset: offset + limit })}>
              <ChevronRight className="size-4" />
            </IconButton>
          </div>
        </div>
      ) : null}
      {bulk ? <BulkDialog action={bulk} ids={selected} lowRatings={rows.filter((r) => selected.includes(r.id) && r.rating <= 2).length} onClose={() => setBulk(null)} onDone={() => setSelected([])} /> : null}
    </div>
  );
}

function BulkDialog({ action, ids, lowRatings, onClose, onDone }: { action: "publish" | "reject"; ids: string[]; lowRatings: number; onClose: () => void; onDone: () => void }) {
  const moderate = useModerate();
  const [reason, setReason] = useState<RejectReason | "">("");
  const submit = () => {
    if (action === "reject" && !reason) return;
    moderate.mutate(
      { ids, action, reason: reason || undefined, confirm: true },
      {
        onSuccess: (r) => {
          toast.success(`${pluralize(r.changed?.length ?? 0, "review")} ${action === "publish" ? "published" : "rejected"}`);
          onDone();
          onClose();
        },
      },
    );
  };
  return (
    <Modal
      title={action === "publish" ? `Publish ${pluralize(ids.length, "review")}?` : `Reject ${pluralize(ids.length, "review")}?`}
      onClose={onClose}
      size="sm"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant={action === "reject" ? "danger" : "primary"} loading={moderate.isPending} disabled={action === "reject" && !reason} onClick={submit}>
            {action === "publish" ? "Publish" : "Reject"}
          </Button>
        </>
      }
    >
      <div className="space-y-3 text-sm text-zinc-600">
        {action === "reject" ? (
          <>
            <p>Rejected reviews stay in DumbMail but don't show anywhere. Reject a review for what it says, never for its rating.</p>
            {lowRatings > 0 ? <Notice tone="amber" title={`${pluralize(lowRatings, "of these is", "of these are")} rated 1 or 2 stars`}>Make sure each one breaks a rule below. Hiding negative reviews is unlawful in the US and EU.</Notice> : null}
            <Field label="Why">
              <Select value={reason} onChange={(e) => setReason(e.target.value as RejectReason | "")}>
                <option value="">Choose a reason</option>
                {(Object.keys(rejectReasonLabels) as RejectReason[]).map((k) => (
                  <option key={k} value={k}>
                    {rejectReasonLabels[k]}
                  </option>
                ))}
              </Select>
            </Field>
          </>
        ) : (
          <p>They'll show on your site and in emails, and count toward each product's stars. Reviews still waiting for an email confirmation are skipped.</p>
        )}
        <InlineError message={moderate.error?.message} />
      </div>
    </Modal>
  );
}

function ReviewCard({ r, timeZone, canManage, canDelete, selected, onSelect }: { r: Review; timeZone: string; canManage: boolean; canDelete: boolean; selected: boolean; onSelect: (on: boolean) => void }) {
  const moderate = useModerate();
  const removePhoto = useRemovePhoto();
  const del = useDeleteReview();
  const [replying, setReplying] = useState(false);
  const [rejecting, setRejecting] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [photo, setPhoto] = useState<string | null>(null);
  const act = (action: ModerateAction, reason?: RejectReason) =>
    moderate.mutate({ ids: [r.id], action, reason }, { onSuccess: (res) => (res.changed?.length ? toast.success(doneText[action]) : toast.info(res.note ?? "Nothing changed")), onError: (e) => toast.error(e.message) });
  const removed = r.status === "removed";
  return (
    <Card padded>
      <div className="flex gap-3">
        {canManage && !removed ? <input type="checkbox" className="mt-1 size-4 shrink-0 accent-accent-700" aria-label={`Select review by ${r.display_name}`} checked={selected} onChange={(e) => onSelect(e.target.checked)} /> : null}
        <div className="min-w-0 flex-1 space-y-2">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <Stars value={r.rating} />
            <ReviewStatusBadge status={r.status} awaitingConfirmation={r.awaiting_confirmation} />
            {r.verified ? <Badge tone="green">Verified buyer</Badge> : r.verified_by_source ? <Badge tone="blue">Marked verified by the source</Badge> : <Badge>Not verified</Badge>}
            {r.imported ? <Badge tone="blue">Imported</Badge> : null}
            {r.source === "public" ? <Badge>From your site</Badge> : null}
            {r.incentivized ? <Badge tone="violet">Got a discount</Badge> : null}
            {r.featured ? <Badge tone="amber">Featured</Badge> : null}
            {r.flags.map((f) => (
              <Badge key={f} tone="red">
                {flagLabel(f)}
              </Badge>
            ))}
          </div>
          <div className="text-xs text-zinc-500">
            <span className="font-medium text-zinc-700">{r.product.name}</span> · {r.display_name}
            {r.author?.email ? ` (${r.author.email})` : ""} · {formatDate(r.reviewed_at, timeZone)}
            {r.edited_at ? " · edited" : ""}
            {r.reject_reason ? ` · rejected: ${rejectReasonLabels[r.reject_reason].toLowerCase()}` : ""}
          </div>
          {removed ? (
            <p className="text-sm text-zinc-500">The author deleted this review.</p>
          ) : (
            <>
              {r.title ? <h3 className="text-sm font-semibold text-zinc-900">{r.title}</h3> : null}
              {r.body ? <p className="whitespace-pre-line text-sm text-zinc-700">{r.body}</p> : <p className="text-sm text-zinc-400">No text, rating only.</p>}
              {r.answers.length ? (
                <dl className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-zinc-600">
                  {r.answers.map((a) => (
                    <div key={a.id}>
                      <dt className="inline text-zinc-500">{a.label}: </dt>
                      <dd className="inline">{a.answer}</dd>
                    </div>
                  ))}
                </dl>
              ) : null}
              {r.photos.length ? (
                <div className="flex flex-wrap gap-2">
                  {r.photos.map((p) => (
                    <div key={p.id} className="relative">
                      <button type="button" onClick={() => setPhoto(p.url)} className="block overflow-hidden rounded-md ring-1 ring-zinc-200" aria-label="Open photo">
                        <img src={p.url} alt="" className="size-16 object-cover" loading="lazy" />
                      </button>
                      {canManage ? (
                        <button
                          type="button"
                          aria-label="Remove this photo"
                          className="absolute -right-1.5 -top-1.5 rounded-full bg-zinc-900 p-0.5 text-white hover:bg-red-600"
                          onClick={() => removePhoto.mutate({ id: r.id, photoId: p.id }, { onSuccess: () => toast.success("Photo removed"), onError: (e) => toast.error(e.message) })}
                        >
                          <X className="size-3" />
                        </button>
                      ) : null}
                    </div>
                  ))}
                </div>
              ) : null}
              {r.reply ? (
                <div className="rounded-md border-l-2 border-accent-600 bg-zinc-50 px-3 py-2 text-sm text-zinc-700">
                  <div className="text-xs font-medium text-zinc-500">Your reply</div>
                  <p className="whitespace-pre-line">{r.reply}</p>
                </div>
              ) : null}
            </>
          )}
          {canManage && !removed ? (
            <div className="flex flex-wrap gap-2 pt-1">
              {r.status !== "published" && !r.awaiting_confirmation ? (
                <Button size="sm" variant="primary" icon={<Check className="size-3.5" />} loading={moderate.isPending} onClick={() => act("publish")}>
                  Publish
                </Button>
              ) : null}
              {r.status === "published" ? (
                <Button size="sm" onClick={() => act("hold")}>
                  Unpublish
                </Button>
              ) : null}
              {r.status !== "rejected" ? (
                <Button size="sm" icon={<X className="size-3.5" />} onClick={() => setRejecting(true)}>
                  Reject
                </Button>
              ) : null}
              <Button size="sm" icon={<MessageSquare className="size-3.5" />} onClick={() => setReplying(true)}>
                {r.reply ? "Edit reply" : "Reply"}
              </Button>
              {r.status === "published" ? (
                <Button size="sm" icon={<Pin className="size-3.5" />} onClick={() => act(r.featured ? "unfeature" : "feature")}>
                  {r.featured ? "Unfeature" : "Feature"}
                </Button>
              ) : null}
              {canDelete ? (
                <Button size="sm" variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => setDeleting(true)}>
                  Delete
                </Button>
              ) : null}
            </div>
          ) : null}
        </div>
      </div>
      {photo ? (
        <Modal title="Photo" onClose={() => setPhoto(null)} size="lg">
          <img src={photo} alt="Review photo" className="mx-auto max-h-[70vh] rounded-md" />
          {r.status !== "published" ? (
            <p className="mt-2 flex items-center gap-1.5 text-xs text-zinc-500">
              <Camera className="size-3.5" /> Visitors can't see this photo until the review is published.
            </p>
          ) : null}
        </Modal>
      ) : null}
      {replying ? <ReplyDialog r={r} onClose={() => setReplying(false)} /> : null}
      {rejecting ? (
        <RejectDialog
          r={r}
          loading={moderate.isPending}
          onClose={() => setRejecting(false)}
          onReject={(reason) => {
            act("reject", reason);
            setRejecting(false);
          }}
        />
      ) : null}
      {deleting ? (
        <ConfirmDialog
          title="Delete this review for good?"
          body="The review and its photos are deleted and can't be brought back. To take it off your site but keep it, reject it instead."
          confirmLabel="Delete"
          danger
          loading={del.isPending}
          onClose={() => setDeleting(false)}
          onConfirm={() => del.mutate(r.id, { onSuccess: () => toast.success("Review deleted"), onError: (e) => toast.error(e.message), onSettled: () => setDeleting(false) })}
        />
      ) : null}
    </Card>
  );
}

const doneText: Record<ModerateAction, string> = { publish: "Published", reject: "Rejected", hold: "Unpublished", feature: "Featured", unfeature: "No longer featured" };

function RejectDialog({ r, loading, onClose, onReject }: { r: Review; loading: boolean; onClose: () => void; onReject: (reason: RejectReason) => void }) {
  const [reason, setReason] = useState<RejectReason | "">("");
  return (
    <Modal
      title="Reject this review?"
      onClose={onClose}
      size="sm"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="danger" disabled={!reason} loading={loading} onClick={() => reason && onReject(reason)}>
            Reject
          </Button>
        </>
      }
    >
      <div className="space-y-3 text-sm text-zinc-600">
        <p>It stays in DumbMail but doesn't show anywhere. Reject a review for what it says, never for its rating.</p>
        {r.rating <= 2 ? <Notice tone="amber" title="This is a low rating">Hiding negative reviews is unlawful in the US and EU. Reject it only if it breaks one of these rules.</Notice> : null}
        <Field label="Why">
          <Select value={reason} onChange={(e) => setReason(e.target.value as RejectReason | "")}>
            <option value="">Choose a reason</option>
            {(Object.keys(rejectReasonLabels) as RejectReason[]).map((k) => (
              <option key={k} value={k}>
                {rejectReasonLabels[k]}
              </option>
            ))}
          </Select>
        </Field>
      </div>
    </Modal>
  );
}

function ReplyDialog({ r, onClose }: { r: Review; onClose: () => void }) {
  const reply = useReply();
  const [text, setText] = useState(r.reply ?? "");
  return (
    <Modal
      title={r.reply ? "Edit your reply" : "Reply to this review"}
      description="Your reply shows under the review on your site. You can't change the review itself."
      onClose={onClose}
      footer={
        <>
          {r.reply ? (
            <Button variant="ghost" onClick={() => reply.mutate({ id: r.id, text: "" }, { onSuccess: () => { toast.success("Reply removed"); onClose(); } })}>
              Remove reply
            </Button>
          ) : null}
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={!text.trim()} loading={reply.isPending} onClick={() => reply.mutate({ id: r.id, text }, { onSuccess: () => { toast.success("Reply saved"); onClose(); } })}>
            Save reply
          </Button>
        </>
      }
    >
      <Field label="Reply" hint={`${text.length} / 2000`}>
        <Textarea rows={5} maxLength={2000} value={text} onChange={(e) => setText(e.target.value)} />
      </Field>
      <InlineError message={reply.error?.message} />
    </Modal>
  );
}
