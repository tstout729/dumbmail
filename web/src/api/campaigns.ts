import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "./client";
import { keys } from "./keys";
import type {
  Audience,
  Campaign,
  CampaignReport,
  CampaignStatus,
  Channel,
  EmailContent,
  ListResponse,
  SmsContent,
  VersionId,
} from "./types";

export type CampaignInput = {
  name: string;
  channel: Channel;
  audience?: Audience;
  content?: EmailContent | SmsContent;
};
/** `ab_test`: an object replaces the whole test, `null` removes it, omitted leaves it as is. */
export type CampaignPatch = Partial<Pick<Campaign, "name" | "audience" | "content" | "ab_test">>;

export const campaignsApi = {
  list: (params?: { status?: CampaignStatus; channel?: Channel }) =>
    api<ListResponse<Campaign>>("GET", withQuery("/api/campaigns", params)),
  get: (id: string) => api<Campaign>("GET", `/api/campaigns/${id}`),
  create: (input: CampaignInput) => api<Campaign>("POST", "/api/campaigns", input),
  update: (id: string, patch: CampaignPatch) => api<Campaign>("PATCH", `/api/campaigns/${id}`, patch),
  remove: (id: string) => api<Campaign>("DELETE", `/api/campaigns/${id}`),
  estimate: (input: { channel: Channel; audience: Audience }) =>
    api<{ count: number }>("POST", "/api/campaigns/estimate", input),
  /** `version` picks which A/B test version to send; omitted sends A. */
  test: (id: string, to: string[], version?: VersionId) =>
    api<{ sent: number }>("POST", `/api/campaigns/${id}/test`, { to, version }),
  schedule: (id: string, send_at: string) => api<Campaign>("POST", `/api/campaigns/${id}/schedule`, { send_at }),
  send: (id: string) => api<Campaign>("POST", `/api/campaigns/${id}/send`),
  cancel: (id: string) => api<Campaign>("POST", `/api/campaigns/${id}/cancel`),
  duplicate: (id: string) => api<Campaign>("POST", `/api/campaigns/${id}/duplicate`),
  report: (id: string) => api<CampaignReport>("GET", `/api/campaigns/${id}/report`),
  /** End an A/B test: send `version` (omitted: the current leader) to everyone not in the test. */
  pickWinner: (id: string, version?: VersionId) => api<Campaign>("POST", `/api/campaigns/${id}/winner`, { version }),
};

/** Poll fast while messages go out, slower while an A/B test waits for results. */
function livePoll(status: CampaignStatus | undefined): number | false {
  if (status === "sending") return 5000;
  if (status === "testing") return 30_000;
  return false;
}

export function useCampaigns() {
  return useQuery({ queryKey: keys.campaigns, queryFn: () => campaignsApi.list(), select: (r) => r.data });
}

export function useCampaign(id: string) {
  return useQuery({
    queryKey: keys.campaign(id),
    queryFn: () => campaignsApi.get(id),
    // Poll while messages are going out or a test is running so status and stats move on their own.
    refetchInterval: (query) => livePoll(query.state.data?.status),
  });
}

/** `status` is the campaign's status; the report polls while it's sending or testing. */
export function useCampaignReport(id: string, enabled: boolean, status?: CampaignStatus) {
  return useQuery({
    queryKey: keys.campaignReport(id),
    queryFn: () => campaignsApi.report(id),
    enabled,
    refetchInterval: livePoll(status),
  });
}

export function useEstimate(input: { channel: Channel; audience: Audience } | null) {
  return useQuery({
    queryKey: keys.estimate(input),
    queryFn: () => campaignsApi.estimate(input as { channel: Channel; audience: Audience }),
    enabled: input !== null && input.audience.include.length > 0,
    placeholderData: keepPreviousData,
    staleTime: 15_000,
  });
}

function useCampaignWriter<V>(fn: (v: V) => Promise<Campaign>) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: (campaign) => {
      qc.setQueryData(keys.campaign(campaign.id), campaign);
      void qc.invalidateQueries({ queryKey: keys.campaigns });
    },
  });
}

export function useCreateCampaign() {
  return useCampaignWriter(campaignsApi.create);
}

export function useUpdateCampaign(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: CampaignPatch) => campaignsApi.update(id, patch),
    onMutate: async (patch) => {
      await qc.cancelQueries({ queryKey: keys.campaign(id) });
      const previous = qc.getQueryData<Campaign>(keys.campaign(id));
      if (previous) qc.setQueryData<Campaign>(keys.campaign(id), { ...previous, ...patch });
      return { previous };
    },
    onError: (_e, _p, ctx) => {
      if (ctx?.previous) qc.setQueryData(keys.campaign(id), ctx.previous);
    },
    onSuccess: (campaign) => {
      qc.setQueryData(keys.campaign(id), campaign);
      void qc.invalidateQueries({ queryKey: keys.campaigns });
    },
  });
}

export function useScheduleCampaign(id: string) {
  return useCampaignWriter((sendAt: string) => campaignsApi.schedule(id, sendAt));
}

export function useSendCampaign(id: string) {
  return useCampaignWriter(() => campaignsApi.send(id));
}

export function useCancelCampaign(id: string) {
  return useCampaignWriter(() => campaignsApi.cancel(id));
}

export function useDuplicateCampaign() {
  return useCampaignWriter(campaignsApi.duplicate);
}

export function useTestCampaign(id: string) {
  return useMutation({ mutationFn: ({ to, version }: { to: string[]; version?: VersionId }) => campaignsApi.test(id, to, version) });
}

/** Send an A/B test's winner (omitted: the leader) to everyone else. */
export function usePickWinner(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (version?: VersionId) => campaignsApi.pickWinner(id, version),
    onSuccess: (campaign) => qc.setQueryData(keys.campaign(id), campaign),
    // Refresh either way: a 409 means someone (or the scheduler) already picked.
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: keys.campaign(id) });
      void qc.invalidateQueries({ queryKey: keys.campaignReport(id) });
      void qc.invalidateQueries({ queryKey: keys.campaigns });
    },
  });
}

export function useDeleteCampaign() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: campaignsApi.remove,
    onMutate: async (id) => {
      await qc.cancelQueries({ queryKey: keys.campaigns });
      const previous = qc.getQueryData<ListResponse<Campaign>>(keys.campaigns);
      if (previous) {
        qc.setQueryData<ListResponse<Campaign>>(keys.campaigns, {
          data: previous.data.filter((c) => c.id !== id),
          total: previous.total - 1,
        });
      }
      return { previous };
    },
    onError: (_e, _id, ctx) => {
      if (ctx?.previous) qc.setQueryData(keys.campaigns, ctx.previous);
    },
    onSettled: () => qc.invalidateQueries({ queryKey: keys.campaigns }),
  });
}
