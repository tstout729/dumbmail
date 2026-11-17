import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import { toast } from "../../lib/toast";
import type { SendStatus, TransactionalSend, TransactionalSettings, TransactionalTemplate } from "./types";

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
    queryFn: () => api<TransactionalSend>("GET", `/api/transactional/sends/${id}`),
    enabled: id !== null,
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

/** Owner: turn transactional email on (after accepting what it's for) or off. */
export function useSetTransactionalEnabled() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: { enabled: boolean; acknowledge?: boolean }) => api<TransactionalSettings>("PATCH", "/api/transactional/settings", input),
    onSuccess: (s) => {
      qc.setQueryData(keys.settings, s);
      toast.success(s.enabled ? "Transactional email is on" : "Transactional email is off");
    },
  });
}
