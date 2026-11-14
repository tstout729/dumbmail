import { Outlet, useNavigation } from "react-router";
import { Toaster } from "./components/ui";

/** Thin top progress bar while a lazy page chunk loads, instead of a full-page spinner. */
export function RootLayout() {
  const navigation = useNavigation();
  return (
    <>
      {navigation.state === "loading" ? (
        <div className="fixed inset-x-0 top-0 z-[70] h-0.5 overflow-hidden">
          <div className="pl-skeleton h-full w-full !bg-accent-500" />
        </div>
      ) : null}
      <Outlet />
      <Toaster />
    </>
  );
}
