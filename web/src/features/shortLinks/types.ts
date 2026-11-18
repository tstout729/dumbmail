/** Settings → Texting: whether links in texts are shortened (`GET /api/short-links/settings`). */
export type ShortLinkSettings = {
  enabled: boolean;
  /** A short link of the real length on the account's link domain. */
  sample: string;
  link_length: number;
  /** False only briefly after an upgrade, while short links wait for a database index. */
  ready: boolean;
};
