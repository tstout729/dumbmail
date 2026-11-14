import { Compass } from "lucide-react";
import { ButtonLink, EmptyState, Page } from "../components/ui";

export function NotFoundPage() {
  return (
    <Page>
      <EmptyState
        icon={<Compass className="size-5" />}
        title="Page not found"
        description="That page doesn't exist or was moved."
        action={<ButtonLink to="/" variant="primary">Go home</ButtonLink>}
      />
    </Page>
  );
}
