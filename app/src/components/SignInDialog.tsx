import { Copy, ExternalLink } from "lucide-react";
import { useEffect, useState } from "react";
import type { DeviceCodePrompt } from "../gen/DeviceCodePrompt";
import { api } from "../lib/api";
import { toast } from "../lib/store";
import { copyAndTell } from "../lib/clipboard";

type Dialog = { status: "closed" } | { status: "code"; prompt: DeviceCodePrompt; until: number } | { status: "failed"; message: string };

function remaining(until: number): string {
  const seconds = Math.max(0, Math.round((until - Date.now()) / 1000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

/** Microsoft Entra ID device-code sign-in, shown when a connection needs a token and the Azure CLI can't provide one. */
export function SignInDialog() {
  const [dialog, setDialog] = useState<Dialog>({ status: "closed" });
  const [, tick] = useState(0);

  useEffect(() => {
    let unsubscribe: (() => void) | undefined;
    let gone = false;
    api()
      .then((client) =>
        client.onSignIn((event) => {
          if (event.state === "code") setDialog({ status: "code", prompt: event.prompt, until: Date.now() + event.prompt.expires_in_secs * 1000 });
          if (event.state === "done") {
            setDialog({ status: "closed" });
            toast("Signed in to Microsoft Entra ID");
          }
          if (event.state === "failed") setDialog((d) => (d.status === "code" ? { status: "failed", message: event.message } : d));
        }),
      )
      .then((off) => (gone ? off() : (unsubscribe = off)));
    return () => {
      gone = true;
      unsubscribe?.();
    };
  }, []);

  useEffect(() => {
    if (dialog.status !== "code") return;
    const timer = setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(timer);
  }, [dialog.status]);

  if (dialog.status === "closed") return null;
  const cancel = async () => {
    setDialog({ status: "closed" });
    await (await api()).cancelSignIn();
  };

  return (
    <div className="overlay" onKeyDown={(e) => e.key === "Escape" && cancel()}>
      <div className="dialog" role="dialog" aria-modal="true" aria-labelledby="sign-in-title">
        <h2 id="sign-in-title">Sign in to Microsoft Entra ID</h2>
        {dialog.status === "code" ? (
          <>
            <p>This connection signs in with Entra ID and the Azure CLI isn't available. Open Microsoft's sign-in page and enter this code:</p>
            <div className="device-code">
              <span className="code">{dialog.prompt.user_code}</span>
              <button className="btn ghost" onClick={() => copyAndTell(dialog.prompt.user_code, "Code copied")}>
                <Copy size={13} /> Copy
              </button>
            </div>
            <div className="actions">
              <button className="btn primary" autoFocus onClick={async () => (await api()).openSignInPage()}>
                <ExternalLink size={13} /> Open {dialog.prompt.verification_uri.replace("https://", "")}
              </button>
              <button className="btn ghost" onClick={cancel}>
                Cancel
              </button>
            </div>
            <div className="waiting">
              <span className="spinner" /> Waiting for you to finish · code expires in {remaining(dialog.until)}
            </div>
          </>
        ) : (
          <>
            <p>{dialog.message}</p>
            <div className="actions">
              <button className="btn" autoFocus onClick={() => setDialog({ status: "closed" })}>
                Close
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
