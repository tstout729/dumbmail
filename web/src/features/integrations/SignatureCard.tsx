import { useState } from "react";
import { errorMessage } from "../../api/client";
import { Button, Card, Field, InlineError, Input, Select } from "../../components/ui";
import { toast } from "../../lib/toast";
import { useUpdateIntegration } from "./api";
import type { Integration, Signature } from "./types";

type Kind = "none" | "plain" | "stripe" | "svix" | "header" | "query";

function kindOf(s: Signature): Kind {
  if (s.type === "none") return "none";
  if (s.type === "shared_secret") return s.query ? "query" : "header";
  return s.format ?? "plain";
}

const kinds: { value: Kind; label: string; hint: string }[] = [
  { value: "none", label: "No check", hint: "Anyone with the URL can send. Use only for apps that can't sign webhooks." },
  { value: "plain", label: "HMAC signature in a header", hint: "An HMAC of the raw body, like GitHub's X-Hub-Signature-256 or Shopify's X-Shopify-Hmac-Sha256." },
  { value: "stripe", label: "Stripe or Calendly style (t=…,v1=…)", hint: "HMAC-SHA256 of timestamp.body, with a 5-minute tolerance." },
  { value: "svix", label: "Svix / Standard Webhooks (Guesty)", hint: "v1,<base64> signatures over id.timestamp.body, with a whsec_ secret." },
  { value: "header", label: "Shared secret in a header", hint: "The app sends the secret itself in a header." },
  { value: "query", label: "Shared secret in the URL", hint: "The app adds the secret as a query parameter." },
];

export function SignatureCard({ integration: i, secretHelp }: { integration: Integration; secretHelp: { label: string; help: string } | null }) {
  const save = useUpdateIntegration(i.id);
  const s = i.signature;
  const [kind, setKind] = useState<Kind>(kindOf(s));
  const [header, setHeader] = useState(s.type === "hmac" ? s.header : s.type === "shared_secret" ? (s.header ?? "") : "");
  const [query, setQuery] = useState(s.type === "shared_secret" ? (s.query ?? "") : "");
  const [algorithm, setAlgorithm] = useState<"sha256" | "sha1">(s.type === "hmac" ? (s.algorithm ?? "sha256") : "sha256");
  const [encoding, setEncoding] = useState<"hex" | "base64">(s.type === "hmac" ? (s.encoding ?? "hex") : "hex");
  const [prefix, setPrefix] = useState(s.type === "hmac" ? (s.prefix ?? "") : "");
  const [timestampHeader, setTimestampHeader] = useState(s.type === "hmac" ? (s.timestamp_header ?? "") : "");
  const [secret, setSecret] = useState("");

  const build = (): Signature => {
    switch (kind) {
      case "none":
        return { type: "none" };
      case "header":
        return { type: "shared_secret", header: header.trim() };
      case "query":
        return { type: "shared_secret", query: query.trim() };
      case "stripe":
        return { type: "hmac", format: "stripe", header: header.trim() || "Stripe-Signature" };
      case "svix":
        return { type: "hmac", format: "svix", header: header.trim() || "svix-signature" };
      case "plain": {
        const sig: Signature = { type: "hmac", header: header.trim(), algorithm, encoding };
        if (prefix.trim()) sig.prefix = prefix.trim();
        if (timestampHeader.trim()) sig.timestamp_header = timestampHeader.trim();
        return sig;
      }
    }
  };

  const onSave = () => {
    save.mutate(secret ? { signature: build(), signature_secret: secret } : { signature: build() }, {
      onSuccess: () => {
        setSecret("");
        toast.success("Signature check saved");
      },
    });
  };

  return (
    <Card title="Signature check">
      <div className="space-y-4">
        <Field label="How the app signs its webhooks" hint={kinds.find((k) => k.value === kind)?.hint}>
          <Select value={kind} onChange={(e) => setKind(e.target.value as Kind)}>
            {kinds.map((k) => (
              <option key={k.value} value={k.value}>
                {k.label}
              </option>
            ))}
          </Select>
        </Field>
        {kind === "query" ? (
          <Field label="Query parameter">
            <Input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="token" />
          </Field>
        ) : kind !== "none" ? (
          <Field label="Header" hint={kind === "stripe" ? "Stripe-Signature, or Calendly-Webhook-Signature" : kind === "svix" ? "svix-signature, or webhook-signature" : undefined}>
            <Input value={header} onChange={(e) => setHeader(e.target.value)} placeholder={kind === "stripe" ? "Stripe-Signature" : kind === "svix" ? "svix-signature" : "X-Signature"} />
          </Field>
        ) : null}
        {kind === "plain" ? (
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="Algorithm">
              <Select value={algorithm} onChange={(e) => setAlgorithm(e.target.value === "sha1" ? "sha1" : "sha256")}>
                <option value="sha256">HMAC-SHA256</option>
                <option value="sha1">HMAC-SHA1</option>
              </Select>
            </Field>
            <Field label="Encoding">
              <Select value={encoding} onChange={(e) => setEncoding(e.target.value === "base64" ? "base64" : "hex")}>
                <option value="hex">Hex</option>
                <option value="base64">Base64</option>
              </Select>
            </Field>
            <Field label="Prefix (optional)" hint="Text before the signature, e.g. sha256=">
              <Input value={prefix} onChange={(e) => setPrefix(e.target.value)} placeholder="sha256=" />
            </Field>
            <Field label="Timestamp header (optional)" hint="If set, the signed text is timestamp.body">
              <Input value={timestampHeader} onChange={(e) => setTimestampHeader(e.target.value)} placeholder="X-Timestamp" />
            </Field>
          </div>
        ) : null}
        {kind !== "none" ? (
          <Field
            label={secretHelp?.label ?? "Signing secret"}
            hint={i.signature_secret.saved ? `Saved (${i.signature_secret.hint ?? "hidden"}). Type a new one to replace it.` : (secretHelp?.help ?? "Copy it from the app's webhook settings. It's stored encrypted and never shown again.")}
          >
            <Input type="password" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} placeholder={i.signature_secret.saved ? "••••••••" : "Paste the secret"} />
          </Field>
        ) : null}
        <InlineError message={save.error ? errorMessage(save.error) : null} />
        <div className="flex justify-end">
          <Button variant="primary" size="sm" loading={save.isPending} onClick={onSave}>
            Save signature check
          </Button>
        </div>
      </div>
    </Card>
  );
}
