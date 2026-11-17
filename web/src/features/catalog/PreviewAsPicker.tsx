import { useId, useState } from "react";
import { useProfiles } from "../../api";
import { Input } from "../../components/ui";
import { fullName } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";

const label = (p: { email: string | null; phone_number: string | null; external_id: string | null; id: string }) => p.email ?? p.phone_number ?? p.external_id ?? p.id;

/** Picks a real profile for an email preview, so product blocks and recommendations show what that person would get. */
export function PreviewAsPicker({ onChange }: { onChange: (profileId: string | undefined) => void }) {
  const listId = useId();
  const [text, setText] = useState("");
  const q = useDebouncedValue(text.trim(), 250);
  const profiles = useProfiles({ q: q || undefined, limit: 10 });
  const options = profiles.data?.data ?? [];
  return (
    <label className="flex min-w-0 items-center gap-1.5 text-xs text-zinc-500">
      <span className="shrink-0">Preview as</span>
      <Input
        value={text}
        list={listId}
        placeholder="Sample person"
        onChange={(e) => {
          const v = e.target.value;
          setText(v);
          onChange(options.find((p) => label(p) === v)?.id);
        }}
        className="h-7 w-44 min-w-0 text-xs"
        aria-label="Preview as profile"
      />
      <datalist id={listId}>
        {options.map((p) => (
          <option key={p.id} value={label(p)}>
            {fullName(p)}
          </option>
        ))}
      </datalist>
    </label>
  );
}
