import type { TileStatus } from "../gen/TileStatus";

export const STATUS_LABEL: Record<TileStatus, string> = {
  bad: "Needs a look now",
  watch: "Worth a look",
  good: "Fine",
  info: "For context",
  unavailable: "Not available here",
};
