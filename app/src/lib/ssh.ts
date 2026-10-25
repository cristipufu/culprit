import type { SshConfig } from "../gen/SshConfig";

/** Editing text stays separate from the validated connection model, especially empty or incomplete port inputs. */
export type SshDraft = { destination: string; port: string; identityFile: string };

export function sshDraft(config: SshConfig | null | undefined): SshDraft | null {
  return config ? { destination: config.destination, port: config.port?.toString() ?? "", identityFile: config.identity_file ?? "" } : null;
}

export function sshConfig(draft: SshDraft | null): SshConfig | null {
  if (!draft) return null;
  if (!draft.destination.trim()) throw new Error("Enter an SSH host or configuration alias");
  const port = draft.port.trim();
  if (port && (!/^[0-9]+$/.test(port) || !Number.isSafeInteger(Number(port)) || Number(port) < 1 || Number(port) > 65535)) {
    throw new Error("The SSH port must be between 1 and 65535");
  }
  return { destination: draft.destination.trim(), port: port ? Number(port) : null, identity_file: draft.identityFile.trim() || null };
}
