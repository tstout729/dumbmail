import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { api, withQuery } from "./client";
import { keys } from "./keys";
import type {
  AvailableNumber,
  PhoneNumber,
  PhoneNumberType,
  PhoneNumbersResponse,
  SmsConnectInput,
  TollFreeVerification,
} from "./types";

export type AvailableQuery = { type: PhoneNumberType; area_code?: string };

export const smsApi = {
  numbers: () => api<PhoneNumbersResponse>("GET", "/api/phone-numbers"),
  available: (q: AvailableQuery) => api<{ data: AvailableNumber[] }>("GET", withQuery("/api/phone-numbers/available", q)),
  claim: (phone_number: string) => api<PhoneNumber>("POST", "/api/phone-numbers", { phone_number }),
  release: (id: string) => api<{ ok: true }>("DELETE", `/api/phone-numbers/${id}`),
  verify: (id: string, input: TollFreeVerification) => api<PhoneNumber>("POST", `/api/phone-numbers/${id}/verification`, input),
  connect: (input: SmsConnectInput) => api<PhoneNumbersResponse>("POST", "/api/sms/connect", input),
  disconnect: () => api<PhoneNumbersResponse>("DELETE", "/api/sms/connect"),
};

export function usePhoneNumbers() {
  return useQuery({ queryKey: keys.phoneNumbers, queryFn: smsApi.numbers });
}

/** Runs only once the user has asked for a search (`query` non-null). */
export function useAvailableNumbers(query: AvailableQuery | null) {
  return useQuery({
    queryKey: keys.availableNumbers(query ?? {}),
    queryFn: () => smsApi.available(query ?? { type: "toll_free" }),
    enabled: query !== null,
    staleTime: 60_000,
    select: (r) => r.data,
  });
}

/** The SMS number is also the account's `sms_from` and part of the sending status, so refresh those too. */
function refreshSmsState(qc: QueryClient, numbers = true) {
  if (numbers) void qc.invalidateQueries({ queryKey: keys.phoneNumbers });
  void qc.invalidateQueries({ queryKey: keys.me });
  void qc.invalidateQueries({ queryKey: keys.sending });
}

export function useClaimNumber() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: smsApi.claim,
    onSuccess: () => {
      refreshSmsState(qc);
      // Claiming charges the first month from the balance.
      void qc.invalidateQueries({ queryKey: keys.billing });
    },
  });
}

export function useReleaseNumber() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: smsApi.release, onSuccess: () => refreshSmsState(qc) });
}

export function useSubmitVerification() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, input }: { id: string; input: TollFreeVerification }) => smsApi.verify(id, input),
    meta: { silentError: true },
    onSuccess: (number) => {
      qc.setQueryData<PhoneNumbersResponse>(keys.phoneNumbers, (old) =>
        old ? { ...old, data: old.data.map((n) => (n.id === number.id ? number : n)) } : old,
      );
      refreshSmsState(qc);
    },
  });
}

export function useConnectSms() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: smsApi.connect,
    meta: { silentError: true },
    onSuccess: (data) => {
      qc.setQueryData(keys.phoneNumbers, data);
      refreshSmsState(qc, false);
    },
  });
}

export function useDisconnectSms() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: smsApi.disconnect,
    onSuccess: (data) => {
      qc.setQueryData(keys.phoneNumbers, data);
      refreshSmsState(qc, false);
    },
  });
}
