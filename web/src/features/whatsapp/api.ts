import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type {
  NewTemplate,
  WhatsappContent,
  WhatsappConversation,
  WhatsappConversationDetail,
  WhatsappPreview,
  WhatsappReplyResult,
  WhatsappReport,
  WhatsappSettings,
  WhatsappSettingsPatch,
  WhatsappTemplate,
} from "./types";

export const whatsappKeys = {
  settings: ["whatsapp", "settings"] as const,
  templates: ["whatsapp", "templates"] as const,
  preview: (content: WhatsappContent | null, profileId?: string) => ["whatsapp", "preview", content, profileId] as const,
  report: (q: { campaign_id?: string; flow_id?: string }) => ["whatsapp", "report", q] as const,
  conversations: (status: string) => ["whatsapp", "conversations", status] as const,
  conversation: (id: string) => ["whatsapp", "conversation", id] as const,
};

/** New messages arrive on their own, so conversation views refresh while open. */
const LIVE = 15_000;

export function useWhatsappSettings() {
  return useQuery({ queryKey: whatsappKeys.settings, queryFn: () => api<WhatsappSettings>("GET", "/api/whatsapp/settings") });
}

export function useUpdateWhatsappSettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: WhatsappSettingsPatch) => api<WhatsappSettings>("PATCH", "/api/whatsapp/settings", patch),
    onSuccess: (s) => qc.setQueryData(whatsappKeys.settings, s),
  });
}

export function useDisconnectWhatsapp() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api<WhatsappSettings>("DELETE", "/api/whatsapp/settings"),
    onSuccess: (s) => qc.setQueryData(whatsappKeys.settings, s),
  });
}

export function useCheckWhatsapp() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api<WhatsappSettings>("POST", "/api/whatsapp/settings/check"),
    onSuccess: (s) => qc.setQueryData(whatsappKeys.settings, s),
  });
}

export function useRotateVerifyToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api<WhatsappSettings>("POST", "/api/whatsapp/settings/verify-token"),
    onSuccess: (s) => qc.setQueryData(whatsappKeys.settings, s),
  });
}

export function useAddTestNumber() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (phone_number: string) => api<{ id: string; phone_number: string; code: string; note: string }>("POST", "/api/whatsapp/test-numbers", { phone_number }),
    onSuccess: () => void qc.invalidateQueries({ queryKey: whatsappKeys.settings }),
  });
}

export function useRemoveTestNumber() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api<{ ok: boolean }>("DELETE", `/api/whatsapp/test-numbers/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: whatsappKeys.settings }),
  });
}

export function useWhatsappTemplates(enabled = true) {
  return useQuery({
    queryKey: whatsappKeys.templates,
    queryFn: () => api<{ templates: WhatsappTemplate[] }>("GET", "/api/whatsapp/templates"),
    enabled,
  });
}

export function useSyncTemplates() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api<{ templates: WhatsappTemplate[]; read: number }>("POST", "/api/whatsapp/templates/sync"),
    onSuccess: (t) => {
      qc.setQueryData(whatsappKeys.templates, { templates: t.templates });
      void qc.invalidateQueries({ queryKey: whatsappKeys.settings });
    },
  });
}

export function useCreateTemplate() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (t: NewTemplate) => api<WhatsappTemplate>("POST", "/api/whatsapp/templates", { ...t, confirm: true }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: whatsappKeys.templates });
      void qc.invalidateQueries({ queryKey: whatsappKeys.settings });
    },
  });
}

/** The template filled for a sample person (or the given profile), as the editors show it. */
export function useWhatsappPreview(content: WhatsappContent | null, profileId?: string) {
  return useQuery({
    queryKey: whatsappKeys.preview(content, profileId),
    queryFn: () => api<WhatsappPreview>("POST", "/api/render", { channel: "whatsapp", content, profile_id: profileId }),
    enabled: content !== null && content.template_id !== null,
    placeholderData: keepPreviousData,
    retry: false,
  });
}

export function useSendTestWhatsapp() {
  return useMutation({
    mutationFn: (input: { to: string; content: WhatsappContent }) => api<{ sent: boolean; message_id: string; to: string }>("POST", "/api/whatsapp/test", input),
  });
}

export function useWhatsappReport(q: { campaign_id?: string; flow_id?: string }) {
  return useQuery({ queryKey: whatsappKeys.report(q), queryFn: () => api<WhatsappReport>("GET", withQuery("/api/whatsapp/report", q)) });
}

export function useWhatsappConversations(status: "open" | "closed") {
  return useQuery({
    queryKey: whatsappKeys.conversations(status),
    queryFn: () => api<{ conversations: WhatsappConversation[] }>("GET", withQuery("/api/whatsapp/conversations", { status, limit: 100 })),
    refetchInterval: LIVE,
  });
}

export function useWhatsappConversation(id: string) {
  return useQuery({
    queryKey: whatsappKeys.conversation(id),
    queryFn: () => api<WhatsappConversationDetail>("GET", `/api/whatsapp/conversations/${id}`),
    refetchInterval: LIVE,
    enabled: id !== "",
  });
}

export function useMarkWhatsappRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api<{ ok: boolean }>("POST", `/api/whatsapp/conversations/${id}/read`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["whatsapp", "conversations"] }),
  });
}

export function useSetWhatsappStatus(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (status: "open" | "closed") => api<{ ok: boolean }>("POST", `/api/whatsapp/conversations/${id}/status`, { status }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["whatsapp", "conversations"] });
      void qc.invalidateQueries({ queryKey: whatsappKeys.conversation(id) });
    },
  });
}

export function useWhatsappReply(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: { text?: string; template?: WhatsappContent }) => api<WhatsappReplyResult>("POST", `/api/whatsapp/conversations/${id}/reply`, { ...input, confirm: true }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: whatsappKeys.conversation(id) });
      void qc.invalidateQueries({ queryKey: ["whatsapp", "conversations"] });
    },
  });
}
