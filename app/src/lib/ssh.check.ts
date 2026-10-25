import { sshConfig, sshDraft } from "./ssh";

function check(ok: boolean, message: string) { if (!ok) throw new Error(message); }

check(sshConfig(null) === null && sshDraft(undefined) === null, "Disabled SSH must stay absent");
for (const port of ["0", "65536", "NaN", "Infinity", "-1", "1.5", "1e3", "22x", "999999999999999999999"]) {
  let rejected = false;
  try { sshConfig({ destination: "bastion", port, identityFile: "" }); } catch { rejected = true; }
  check(rejected, `Invalid SSH port ${port} must not become a default port through JSON serialization`);
}
for (const port of ["", "1", "22", "65535"]) {
  const config = sshConfig({ destination: "  reader@bastion  ", port, identityFile: "  ~/key with spaces  " });
  check(config?.destination === "reader@bastion" && config.identity_file === "~/key with spaces", "Trim fields without splitting paths");
  check(config?.port === (port ? Number(port) : null), "An empty port must defer to OpenSSH configuration");
  check(sshDraft(config)?.port === port, "Editing a saved port must preserve it");
}
let rejected = false;
try { sshConfig({ destination: "  ", port: "22", identityFile: "" }); } catch { rejected = true; }
check(rejected, "Enabled SSH requires a destination");
console.log("SSH drafts: absent routes, incomplete and out-of-range ports, saved ports and identity paths passed");
