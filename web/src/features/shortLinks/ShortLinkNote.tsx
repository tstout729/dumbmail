import { Link } from "react-router";
import { useShortLinkSettings } from "./api";

const link = /https?:\/\/\S/;

/** A line under the text editor's segment count saying what happens to the links in `body`. */
export function ShortLinkNote({ body }: { body: string }) {
  const settings = useShortLinkSettings();
  if (!link.test(body) || !settings.data) return null;
  return (
    <Link to="/settings/texting" className="text-zinc-500 underline decoration-zinc-300 hover:text-zinc-800">
      {settings.data.enabled ? `Links shortened to ${settings.data.link_length} characters` : "Links aren't shortened"}
    </Link>
  );
}
