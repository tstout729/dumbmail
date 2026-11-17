import { keepPreviousData, useInfiniteQuery, useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type {
  ConversationDetail,
  ConversationList,
  ConversationStatus,
  InboxSettings,
  InboxSettingsPatch,
  ReplyDryRun,
  ReplySent,
  SavedReply,
  SmsKeyword,
  SmsKeywordInput,
  Unread,
} from "./types";

export type ConversationFilter = { status: ConversationStatus | "all"; unread: boolean; q: string };

export const inboxKeys = {
  all: ["inbox"] as const,
  conversations: (f: ConversationFilter) => ["inbox", "conversations", f] as const,
  conversation: (id: string) => ["inbox", "conversation", id] as const,
  unread: ["inbox", "unread"] as const,
  keywords: ["inbox", "keywords"] as const,
  savedReplies: ["inbox", "saved-replies"] as const,
  settings: ["inbox", "settings"] as const,
};

/** New texts arrive on their own, so inbox views refresh every 15 seconds while open. */
const LIVE = 15_000;

const PAGE = 50;

/** Conversations, a page at a time ("Show more" loads the next). */
export function useConversations(f: ConversationFilter) {
  return useInfiniteQuery({
    queryKey: inboxKeys.conversations(f),
    queryFn: ({ pageParam }) =>
      api<ConversationList>("GET", withQuery("/api/inbox/conversations", { status: f.status, unread: f.unread || undefined, q: f.q, limit: PAGE, cursor: pageParam ?? undefined })),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    placeholderData: keepPreviousData,
    refetchInterval: LIVE,
  });
}

/** A thread: the newest texts first, then older pages (`cursor`). */
export function useConversation(id: string) {
  return useInfiniteQuery({
    queryKey: inboxKeys.conversation(id),
    queryFn: ({ pageParam }) => api<ConversationDetail>("GET", withQuery(`/api/inbox/conversations/${id}`, { limit: PAGE, cursor: pageParam ?? undefined })),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    refetchInterval: LIVE,
  });
}

export function useUnread() {
  return useQuery({ queryKey: inboxKeys.unread, queryFn: () => api<Unread>("GET", "/api/inbox/unread"), refetchInterval: 30_000 });
}

function refreshInbox(qc: QueryClient) {
  void qc.invalidateQueries({ queryKey: inboxKeys.all });
}

export function useMarkRead() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: (id: string) => api<{ ok: true }>("POST", `/api/inbox/conversations/${id}/read`, {}), onSuccess: () => refreshInbox(qc) });
}

export function useSetConversationStatus() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, status }: { id: string; status: ConversationStatus }) => api<{ ok: true }>("POST", `/api/inbox/conversations/${id}/status`, { status }),
    onSuccess: () => refreshInbox(qc),
  });
}

export function usePreviewReply() {
  return useMutation({
    mutationFn: ({ id, body }: { id: string; body: string }) => api<ReplyDryRun>("POST", `/api/inbox/conversations/${id}/reply`, { body }),
    meta: { silentError: true },
  });
}

export function useSendReply() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, body }: { id: string; body: string }) => api<ReplySent>("POST", `/api/inbox/conversations/${id}/reply`, { body, confirm: true }),
    meta: { silentError: true },
    onSuccess: () => refreshInbox(qc),
  });
}

export function useKeywords() {
  return useQuery({ queryKey: inboxKeys.keywords, queryFn: () => api<{ data: SmsKeyword[]; total: number }>("GET", "/api/inbox/keywords"), select: (r) => r.data });
}

export function useSaveKeyword() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, input }: { id?: string; input: SmsKeywordInput }) =>
      id ? api<SmsKeyword>("PATCH", `/api/inbox/keywords/${id}`, input) : api<SmsKeyword>("POST", "/api/inbox/keywords", input),
    meta: { silentError: true },
    onSuccess: () => void qc.invalidateQueries({ queryKey: inboxKeys.keywords }),
  });
}

export function useDeleteKeyword() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api<{ ok: true }>("DELETE", `/api/inbox/keywords/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: inboxKeys.keywords }),
  });
}

export function useSavedReplies() {
  return useQuery({ queryKey: inboxKeys.savedReplies, queryFn: () => api<{ data: SavedReply[]; total: number }>("GET", "/api/inbox/saved-replies"), select: (r) => r.data });
}

export function useSaveSavedReply() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, name, body }: { id?: string; name: string; body: string }) =>
      id ? api<SavedReply>("PATCH", `/api/inbox/saved-replies/${id}`, { name, body }) : api<SavedReply>("POST", "/api/inbox/saved-replies", { name, body }),
    meta: { silentError: true },
    onSuccess: () => void qc.invalidateQueries({ queryKey: inboxKeys.savedReplies }),
  });
}

export function useDeleteSavedReply() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api<{ ok: true }>("DELETE", `/api/inbox/saved-replies/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: inboxKeys.savedReplies }),
  });
}

export function useInboxSettings() {
  return useQuery({ queryKey: inboxKeys.settings, queryFn: () => api<InboxSettings>("GET", "/api/inbox/settings") });
}

export function useSaveInboxSettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: InboxSettingsPatch) => api<InboxSettings>("PATCH", "/api/inbox/settings", patch),
    meta: { silentError: true },
    onSuccess: (s) => qc.setQueryData(inboxKeys.settings, s),
  });
}
