import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { SendStatus, TransactionalSend, TransactionalSendDetail, TransactionalSettings, TransactionalTemplate } from "./types";

export const pageSize = 50;

const keys = {
  all: ["transactional"] as const,
  sends: (q: object) => ["transactional", "sends", q] as const,
  send: (id: string) => ["transactional", "send", id] as const,
  templates: ["transactional", "templates"] as const,
  settings: ["transactional", "settings"] as const,
};

export function useTransactionalSends(params: { q: string; status: SendStatus | ""; offset: number }) {
  return useQuery({
    queryKey: keys.sends(params),
    queryFn: () => api<{ data: TransactionalSend[] }>("GET", withQuery("/api/transactional/sends", { ...params, limit: pageSize })),
    placeholderData: keepPreviousData,
    select: (r) => r.data,
  });
}

export function useTransactionalSend(id: string | null) {
  return useQuery({
    queryKey: keys.send(id ?? ""),
    queryFn: () => api<TransactionalSendDetail>("GET", `/api/transactional/sends/${id}`),
    enabled: id !== null,
  });
}

export function useResend() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api<TransactionalSend>("POST", `/api/transactional/sends/${id}/resend`),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.all }),
  });
}

export function useTransactionalTemplates() {
  return useQuery({
    queryKey: keys.templates,
    queryFn: () => api<{ data: TransactionalTemplate[] }>("GET", "/api/transactional/templates"),
    select: (r) => r.data,
  });
}

export function useCreateTransactionalTemplate() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: { name: string; content: { subject: string; text?: string; html?: string } }) =>
      api<TransactionalTemplate>("POST", "/api/transactional/templates", input),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: keys.templates });
      void qc.invalidateQueries({ queryKey: ["templates"] });
    },
  });
}

export function useTransactionalSettings() {
  return useQuery({ queryKey: keys.settings, queryFn: () => api<TransactionalSettings>("GET", "/api/transactional/settings"), refetchInterval: 60_000 });
}
