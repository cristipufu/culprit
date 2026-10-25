import { ownedRequests } from "./owned-requests";

export function discoveryRequests(invoke: Parameters<typeof ownedRequests>[0]) {
  return ownedRequests(invoke, "begin_discovery", "cancel_discovery");
}
