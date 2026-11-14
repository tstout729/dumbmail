import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { Form, FormConfig, FormKind, ListResponse } from "./types";

export type FormInput = { name: string; kind: FormKind; list_id: string | null; config: FormConfig };
export type FormPatch = Partial<FormInput & { status: Form["status"] }>;

export const formsApi = {
  list: () => api<ListResponse<Form>>("GET", "/api/forms"),
  get: (id: string) => api<Form>("GET", `/api/forms/${id}`),
  create: (input: FormInput) => api<Form>("POST", "/api/forms", input),
  update: (id: string, patch: FormPatch) => api<Form>("PATCH", `/api/forms/${id}`, patch),
  remove: (id: string) => api<Form>("DELETE", `/api/forms/${id}`),
};

export function useForms() {
  return useQuery({ queryKey: keys.forms, queryFn: formsApi.list, select: (r) => r.data });
}

export function useForm(id: string) {
  return useQuery({ queryKey: keys.form(id), queryFn: () => formsApi.get(id) });
}

export function useCreateForm() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: formsApi.create,
    onSuccess: (form) => {
      qc.setQueryData(keys.form(form.id), form);
      void qc.invalidateQueries({ queryKey: keys.forms });
    },
  });
}

export function useUpdateForm(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: FormPatch) => formsApi.update(id, patch),
    onSuccess: (form) => {
      qc.setQueryData(keys.form(id), form);
      void qc.invalidateQueries({ queryKey: keys.forms });
    },
  });
}

export function useDeleteForm() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: formsApi.remove,
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.forms }),
  });
}
