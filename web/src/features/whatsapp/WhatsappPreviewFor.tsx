import { useWhatsappPreview } from "./api";
import type { WhatsappContent } from "./types";
import { WhatsappPhone } from "./WhatsappEditor";

/** The phone preview of saved WhatsApp content (campaign reports). */
export function WhatsappPreviewFor({ content }: { content: WhatsappContent }) {
  const preview = useWhatsappPreview(content);
  return <WhatsappPhone preview={preview.data ?? null} loading={preview.isFetching} error={preview.error?.message ?? null} empty={!content.template_id} />;
}
