export type HubLauncher = "off" | "button" | "link";
export type HubObjectKind = "booking" | "subscription" | "other";
export type HubFont = "system" | "sans" | "serif";

export type HubSections = {
  orders: boolean;
  bookings: boolean;
  recommendations: boolean;
  preferences: boolean;
  help: boolean;
};

export type HubObjectChoice = { type: string; kind: HubObjectKind; fields: string[] };
export type HubHelpEntry = { question: string; answer: string };

export type HubField = { key: string; label: string | null; type: string; sensitive: boolean; access_like: boolean };
export type HubAvailableType = { key: string; name: string; plural: string; count: number; fields: HubField[] };

export type HubSettings = {
  enabled: boolean;
  launcher: HubLauncher;
  button_label: string;
  sections: HubSections;
  object_types: HubObjectChoice[];
  help: HubHelpEntry[];
  contact_url: string | null;
  contact_label: string | null;
  accent_color: string | null;
  font: HubFont;
  /** The hub's address. */
  url: string;
  /** The site where the hub opens in a panel; null means it opens in a new tab. */
  embed_site: string | null;
  link_domain: { domain: string | null; active: boolean };
  available_types: HubAvailableType[];
};

export type HubSettingsPatch = Partial<Omit<HubSettings, "url" | "embed_site" | "link_domain" | "available_types">>;
