import { useState } from "react";
import { Link } from "react-router";
import { KeyRound, Pencil, Plus, Trash2 } from "lucide-react";
import { errorMessage, useLists } from "../../api";
import {
  Badge,
  Button,
  Checkbox,
  ConfirmDialog,
  EmptyRow,
  EmptyState,
  Field,
  IconButton,
  InlineError,
  Input,
  Modal,
  Notice,
  Page,
  PageHeader,
  Segmented,
  Select,
  SkeletonRows,
  StatusBadge,
  Table,
  TagInput,
  Td,
  Textarea,
  Th,
} from "../../components/ui";
import { toast } from "../../lib/toast";
import { useDeleteKeyword, useInboxSettings, useKeywords, useSaveKeyword } from "./api";
import type { KeywordMatch, SmsKeyword } from "./types";

export function KeywordsPage() {
  const keywords = useKeywords();
  const lists = useLists();
  const [editing, setEditing] = useState<SmsKeyword | "new" | null>(null);
  const [deleting, setDeleting] = useState<SmsKeyword | null>(null);
  const remove = useDeleteKeyword();
  const listName = (id: string | null) => (id ? (lists.data?.find((l) => l.id === id)?.name ?? "a list") : null);

  return (
    <Page>
      <PageHeader
        title="Keywords"
        description="Words people can text you, like JOIN, and what each one does."
        breadcrumbs={[{ label: "Inbox", to: "/inbox" }]}
        actions={
          <Button variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setEditing("new")}>
            New keyword
          </Button>
        }
      />
      <Table>
        <thead>
          <tr>
            <Th>Keyword</Th>
            <Th>What it does</Th>
            <Th>Status</Th>
            <Th className="w-20" />
          </tr>
        </thead>
        <tbody>
          {keywords.isPending ? (
            <SkeletonRows cols={4} rows={3} />
          ) : keywords.error ? (
            <EmptyRow cols={4}>
              <p className="p-4 text-sm text-red-600">{keywords.error.message}</p>
            </EmptyRow>
          ) : keywords.data.length === 0 ? (
            <EmptyRow cols={4}>
              <EmptyState
                icon={<KeyRound className="size-5" />}
                title="No keywords yet"
                description="Add one like JOIN so people can sign up for texts, or HOURS to send your opening times."
                action={
                  <Button variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setEditing("new")}>
                    New keyword
                  </Button>
                }
              />
            </EmptyRow>
          ) : (
            keywords.data.map((k) => (
              <tr key={k.id} className="[&:last-child>td]:border-b-0">
                <Td>
                  <div className="font-mono text-sm font-semibold text-zinc-900">{k.keyword.toUpperCase()}</div>
                  {k.aliases.length > 0 ? <div className="font-mono text-xs text-zinc-500">also {k.aliases.map((a) => a.toUpperCase()).join(", ")}</div> : null}
                  {k.match_type === "starts_with" ? <div className="text-xs text-zinc-500">or any text starting with it</div> : null}
                </Td>
                <Td>
                  <div className="flex flex-wrap gap-1">
                    {k.subscribe ? <Badge tone="green">{k.double_opt_in ? "Subscribes after they reply Y" : "Subscribes to texts"}</Badge> : null}
                    {k.list_id ? <Badge tone="blue">Adds to {listName(k.list_id)}</Badge> : null}
                    {k.reply ? <Badge>Replies</Badge> : null}
                  </div>
                  {k.reply ? <p className="mt-1 line-clamp-2 max-w-md text-xs text-zinc-500">{k.reply}</p> : null}
                </Td>
                <Td>
                  <StatusBadge status={k.enabled ? "active" : "paused"} label={k.enabled ? "On" : "Off"} />
                </Td>
                <Td className="text-right">
                  <div className="flex justify-end gap-1">
                    <IconButton label={`Edit ${k.keyword}`} onClick={() => setEditing(k)}>
                      <Pencil className="size-3.5" />
                    </IconButton>
                    <IconButton label={`Delete ${k.keyword}`} onClick={() => setDeleting(k)}>
                      <Trash2 className="size-3.5" />
                    </IconButton>
                  </div>
                </Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
      <p className="mt-3 text-xs text-zinc-500">
        STOP, START, HELP and similar carrier words always keep their usual meaning and can't be keywords. A flow can start when someone texts a keyword: use the
        trigger metric "Received SMS Reply" with the filter keyword equals JOIN.
      </p>
      {editing ? <KeywordDialog key={editing === "new" ? "new" : editing.id} keyword={editing === "new" ? null : editing} onClose={() => setEditing(null)} /> : null}
      {deleting ? (
        <ConfirmDialog
          title={`Delete ${deleting.keyword.toUpperCase()}?`}
          body="Texts with this word will become ordinary messages in the inbox."
          confirmLabel="Delete"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate(deleting.id, {
              onSuccess: () => {
                setDeleting(null);
                toast.success("Keyword deleted");
              },
            })
          }
          onClose={() => setDeleting(null)}
        />
      ) : null}
    </Page>
  );
}

function KeywordDialog({ keyword, onClose }: { keyword: SmsKeyword | null; onClose: () => void }) {
  const lists = useLists();
  const settings = useInboxSettings();
  const save = useSaveKeyword();
  const [word, setWord] = useState(keyword?.keyword.toUpperCase() ?? "");
  const [aliases, setAliases] = useState<string[]>(keyword?.aliases.map((a) => a.toUpperCase()) ?? []);
  const [match, setMatch] = useState<KeywordMatch>(keyword?.match_type ?? "exact");
  const [subscribe, setSubscribe] = useState(keyword?.subscribe ?? false);
  const [doubleOptIn, setDoubleOptIn] = useState(keyword?.double_opt_in ?? false);
  const [listId, setListId] = useState(keyword?.list_id ?? "");
  const [reply, setReply] = useState(keyword?.reply ?? "");
  const [enabled, setEnabled] = useState(keyword?.enabled ?? true);
  const s = settings.data;
  const needsDisclosure = subscribe && s !== undefined && !s.disclosure_ready;
  const needsSupport = enabled && s !== undefined && !s.support_ready;
  // Sign-up keywords only ever match the whole text, so "join me for lunch?" signs nobody up.
  const matchType: KeywordMatch = subscribe ? "exact" : match;

  const submit = () =>
    save.mutate(
      { id: keyword?.id, input: { keyword: word, aliases, match_type: matchType, subscribe, double_opt_in: subscribe && doubleOptIn, list_id: listId || null, reply, enabled } },
      {
        onSuccess: () => {
          toast.success(keyword ? "Keyword saved" : "Keyword added");
          onClose();
        },
      },
    );

  return (
    <Modal
      title={keyword ? `Edit ${keyword.keyword.toUpperCase()}` : "New keyword"}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" loading={save.isPending} disabled={!word.trim() || needsDisclosure || needsSupport} onClick={submit}>
            Save
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <Field label="Keyword" hint="One word of letters and numbers. Case doesn't matter.">
            <Input value={word} onChange={(e) => setWord(e.target.value.toUpperCase())} maxLength={20} placeholder="JOIN" className="font-mono" />
          </Field>
          <Field label="Other words that do the same (optional)">
            <TagInput value={aliases} onChange={(v) => setAliases(v.map((a) => a.toUpperCase()))} placeholder="VIP, DEALS" />
          </Field>
        </div>
        <Field label="Match" hint={subscribe ? "Sign-up keywords must be the whole text." : undefined}>
          <Segmented
            value={matchType}
            onChange={(m) => {
              if (!subscribe) setMatch(m);
            }}
            options={[
              { value: "exact", label: "The whole text is the word" },
              { value: "starts_with", label: "The text starts with it" },
            ]}
          />
        </Field>
        <div className="space-y-2 rounded-lg border border-zinc-200 p-3">
          <Checkbox
            label="Subscribe them to marketing texts"
            description="Texting the keyword is their sign-up. They're subscribed once the confirmation text (with the required disclosures) goes out, and DumbMail keeps a copy of exactly what it said."
            checked={subscribe} onChange={(e) => setSubscribe(e.target.checked)} />
          {subscribe ? (
            <Checkbox
              className="ml-6"
              label="Ask them to reply Y first (double opt-in)"
              description="We text them a question first. A Y within 7 days of it going out sends the confirmation."
              checked={doubleOptIn}
              onChange={(e) => setDoubleOptIn(e.target.checked)}
            />
          ) : null}
          {needsDisclosure ? (
            <Notice tone="amber" title="Add your program name and message frequency first">
              Sign-up texts must say who you are and how often you text. Set them in <Link to="/settings/inbox" className="underline">Inbox settings</Link>.
            </Notice>
          ) : null}
        </div>
        <Field label="Add them to a list (optional)">
          <Select value={listId} onChange={(e) => setListId(e.target.value)}>
            <option value="">No list</option>
            {(lists.data ?? []).map((l) => (
              <option key={l.id} value={l.id}>
                {l.name}
              </option>
            ))}
          </Select>
        </Field>
        <Field
          label={subscribe ? "Welcome text (optional)" : "Reply"}
          hint={
            subscribe && s
              ? `Sent as: "${s.program_name || "Your program"}: ${reply || "Thanks! You're signed up for texts."} ${s.message_frequency || "…"}. Msg & data rates may apply. Reply HELP for help, STOP to cancel."`
              : "Personalize with {{ first_name|default('there') }}."
          }
        >
          <Textarea value={reply} onChange={(e) => setReply(e.target.value)} rows={3} maxLength={480} placeholder={subscribe ? "Welcome! Here's 10% off: SAVE10" : "We're open 9 to 5, Monday to Friday."} />
        </Field>
        <Checkbox label="On" description="Turn off to stop answering this word without deleting it." checked={enabled} onChange={(e) => setEnabled(e.target.checked)} />
        {needsSupport ? (
          <Notice tone="amber" title="Add a support contact first">
            Anyone who texts HELP gets your program name and how to reach you. Add an email or phone in <Link to="/settings/inbox" className="underline">Inbox settings</Link>.
          </Notice>
        ) : null}
        <InlineError message={save.error ? errorMessage(save.error) : null} />
      </div>
    </Modal>
  );
}
