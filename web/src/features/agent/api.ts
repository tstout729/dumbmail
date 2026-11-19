import { keepPreviousData, useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { AgentConversationDetail, AgentConversationList, AgentSettings, AgentSettingsPatch, AgentTestReply } from "./types";

export const agentKeys = {
  settings: ["agent", "settings"] as const,
  conversations: (handedOff: boolean | null) => ["agent", "conversations", handedOff] as const,
  conversation: (id: string) => ["agent", "conversation", id] as const,
};

export function useAgentSettings() {
  return useQuery({ queryKey: agentKeys.settings, queryFn: () => api<AgentSettings>("GET", "/api/agent/settings") });
}

export function useUpdateAgentSettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (p: AgentSettingsPatch) => api<AgentSettings>("PATCH", "/api/agent/settings", p),
    onSuccess: (s) => qc.setQueryData(agentKeys.settings, s),
  });
}

const PAGE = 25;

/** Conversations, newest first, a page at a time. `handedOff`: null for all. */
export function useAgentConversations(handedOff: boolean | null, enabled = true) {
  return useInfiniteQuery({
    queryKey: agentKeys.conversations(handedOff),
    queryFn: ({ pageParam }) =>
      api<AgentConversationList>("GET", withQuery("/api/agent/conversations", { handed_off: handedOff ?? undefined, limit: PAGE, cursor: pageParam ?? undefined })),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    placeholderData: keepPreviousData,
    refetchInterval: 30_000,
    enabled,
  });
}

export function useAgentConversation(id: string) {
  return useQuery({ queryKey: agentKeys.conversation(id), queryFn: () => api<AgentConversationDetail>("GET", `/api/agent/conversations/${id}`), enabled: !!id, refetchInterval: 30_000 });
}

/** The settings page's test chat (a made-up shopper with a made-up order). */
export function useAgentTest() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (b: { message: string; conversation: string | null }) => api<AgentTestReply>("POST", "/api/agent/test", { message: b.message, conversation: b.conversation ?? undefined }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: agentKeys.settings });
      void qc.invalidateQueries({ queryKey: ["agent", "conversations"] });
    },
  });
}
