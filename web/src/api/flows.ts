import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { Flow, FlowBlueprint, FlowDefinition, FlowReport, FlowStatus, FlowTrigger, ListResponse } from "./types";

export type FlowInput = { name: string; trigger: FlowTrigger; definition: FlowDefinition };

export const flowsApi = {
  list: () => api<ListResponse<Flow>>("GET", "/api/flows"),
  get: (id: string) => api<Flow>("GET", `/api/flows/${id}`),
  create: (input: FlowInput) => api<Flow>("POST", "/api/flows", input),
  update: (id: string, patch: Partial<FlowInput>) => api<Flow>("PATCH", `/api/flows/${id}`, patch),
  remove: (id: string) => api<Flow>("DELETE", `/api/flows/${id}`),
  setStatus: (id: string, status: FlowStatus) => api<Flow>("POST", `/api/flows/${id}/status`, { status }),
  report: (id: string) => api<FlowReport>("GET", `/api/flows/${id}/report`),
  blueprints: () => api<{ data: FlowBlueprint[] }>("GET", "/api/flows/blueprints"),
  fromBlueprint: (input: { key: string; name?: string }) => api<Flow>("POST", "/api/flows/from-blueprint", input),
};

export function useFlows() {
  return useQuery({ queryKey: keys.flows, queryFn: flowsApi.list, select: (r) => r.data });
}

export function useFlow(id: string) {
  return useQuery({ queryKey: keys.flow(id), queryFn: () => flowsApi.get(id) });
}

export function useFlowReport(id: string, enabled: boolean) {
  return useQuery({ queryKey: keys.flowReport(id), queryFn: () => flowsApi.report(id), enabled });
}

export function useBlueprints() {
  return useQuery({
    queryKey: keys.blueprints,
    queryFn: flowsApi.blueprints,
    select: (r) => r.data,
    staleTime: 60 * 60_000,
  });
}

function useFlowWriter<V>(fn: (v: V) => Promise<Flow>) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: (flow) => {
      qc.setQueryData(keys.flow(flow.id), flow);
      void qc.invalidateQueries({ queryKey: keys.flows });
    },
  });
}

export function useCreateFlow() {
  return useFlowWriter(flowsApi.create);
}

export function useCreateFlowFromBlueprint() {
  return useFlowWriter(flowsApi.fromBlueprint);
}

export function useUpdateFlow(id: string) {
  return useFlowWriter((patch: Partial<FlowInput>) => flowsApi.update(id, patch));
}

export function useSetFlowStatus() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, status }: { id: string; status: FlowStatus }) => flowsApi.setStatus(id, status),
    onMutate: async ({ id, status }) => {
      await qc.cancelQueries({ queryKey: keys.flow(id) });
      const previous = qc.getQueryData<Flow>(keys.flow(id));
      if (previous) qc.setQueryData<Flow>(keys.flow(id), { ...previous, status });
      return { previous };
    },
    onError: (_e, { id }, ctx) => {
      if (ctx?.previous) qc.setQueryData(keys.flow(id), ctx.previous);
    },
    onSuccess: (flow) => {
      qc.setQueryData(keys.flow(flow.id), flow);
      void qc.invalidateQueries({ queryKey: keys.flows });
    },
  });
}

export function useDeleteFlow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: flowsApi.remove,
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.flows }),
  });
}
