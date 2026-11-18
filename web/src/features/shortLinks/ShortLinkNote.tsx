import { Link } from "react-router";
import { useShortLinkSettings } from "./api";

const link = /https?:\/\/\S/;

/**
 * A line under the text editor's segment count. It says links are shortened only when the server's
 * preview shortened some (`shortened`: the setting is on, the server issues short links and the text
 * has a link that will be shortened), and that they aren't when the setting is off.
 */
export function ShortLinkNote({ body, shortened }: { body: string; shortened: number | undefined }) {
  const settings = useShortLinkSettings();
  if (!link.test(body) || !settings.data) return null;
  if (shortened && shortened > 0) {
    return (
      <Link to="/settings/texting" className="text-zinc-500 underline decoration-zinc-300 hover:text-zinc-800">
        {shortened === 1 ? "Link" : `${shortened} links`} shortened to {settings.data.link_length} characters
      </Link>
    );
  }
  if (settings.data.enabled) return null;
  return (
    <Link to="/settings/texting" className="text-zinc-500 underline decoration-zinc-300 hover:text-zinc-800">
      Links aren't shortened
    </Link>
  );
}
