import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../../api/client";
import type { ReviewAnswers, SendingReview } from "./types";

const key = ["screening", "review"] as const;

/** The account's sending review. Checked again every 30 seconds while it can't send yet. */
export function useSendingReview() {
  return useQuery({
    queryKey: key,
    queryFn: () => api<SendingReview>("GET", "/api/screening"),
    refetchInterval: (q) => (q.state.data && !q.state.data.can_send ? 30_000 : false),
  });
}

/** Save the three answers and (for a new account) run its check. Takes up to a minute. */
export function useRequestReview() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (a: ReviewAnswers) => api<SendingReview>("POST", "/api/screening/request", a),
    onSuccess: (r) => qc.setQueryData(key, r),
  });
}

export function useRefreshReview() {
  const qc = useQueryClient();
  return () => void qc.invalidateQueries({ queryKey: key });
}
