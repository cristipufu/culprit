import { copyText } from "./api";
import { toast } from "./store";

/** Copy text and say so; when the clipboard refuses, say that instead of failing silently. True when it copied. */
export async function copyAndTell(text: string, message?: string): Promise<boolean> {
  try {
    await copyText(text);
    if (message) toast(message);
    return true;
  } catch {
    toast("Couldn't copy: click the window and try again");
    return false;
  }
}
