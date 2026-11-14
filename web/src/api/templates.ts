import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "./client";
import { keys } from "./keys";
import type { Channel, EmailContent, EmailRender, ListResponse, SmsContent, SmsRender, Template } from "./types";

export type TemplateInput = { name: string; channel: Channel; content: EmailContent | SmsContent };

export type RenderInput =
  | { channel: "email"; content: EmailContent; profile_id?: string }
  | { channel: "sms"; content: SmsContent; profile_id?: string };

export const templatesApi = {
  list: (channel?: Channel) => api<ListResponse<Template>>("GET", withQuery("/api/templates", { channel })),
  get: (id: string) => api<Template>("GET", `/api/templates/${id}`),
  create: (input: TemplateInput) => api<Template>("POST", "/api/templates", input),
  update: (id: string, input: Partial<TemplateInput>) => api<Template>("PATCH", `/api/templates/${id}`, input),
  remove: (id: string) => api<Template>("DELETE", `/api/templates/${id}`),
  renderEmail: (content: EmailContent, profile_id?: string) =>
    api<EmailRender>("POST", "/api/render", { channel: "email", content, profile_id }),
  renderSms: (content: SmsContent, profile_id?: string) =>
    api<SmsRender>("POST", "/api/render", { channel: "sms", content, profile_id }),
};

export function useTemplates(channel?: Channel) {
  return useQuery({ queryKey: keys.templates(channel), queryFn: () => templatesApi.list(channel), select: (r) => r.data });
}

export function useTemplate(id: string) {
  return useQuery({ queryKey: keys.template(id), queryFn: () => templatesApi.get(id) });
}

export function useCreateTemplate() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: templatesApi.create,
    onSuccess: (t) => {
      qc.setQueryData(keys.template(t.id), t);
      void qc.invalidateQueries({ queryKey: keys.templatesAll });
    },
  });
}

export function useUpdateTemplate(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: Partial<TemplateInput>) => templatesApi.update(id, input),
    onSuccess: (t) => {
      qc.setQueryData(keys.template(id), t);
      void qc.invalidateQueries({ queryKey: keys.templatesAll });
    },
  });
}

export function useDeleteTemplate() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: templatesApi.remove,
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.templatesAll }),
  });
}

/** Server-side email render. Pass already-debounced content. */
export function useEmailRender(content: EmailContent | null) {
  return useQuery({
    queryKey: keys.render({ channel: "email", content }),
    queryFn: () => templatesApi.renderEmail(content as EmailContent),
    enabled: content !== null,
    placeholderData: keepPreviousData,
    staleTime: 60_000,
    retry: false,
  });
}

export function useSmsRender(content: SmsContent | null) {
  return useQuery({
    queryKey: keys.render({ channel: "sms", content }),
    queryFn: () => templatesApi.renderSms(content as SmsContent),
    enabled: content !== null,
    placeholderData: keepPreviousData,
    staleTime: 60_000,
    retry: false,
  });
}
