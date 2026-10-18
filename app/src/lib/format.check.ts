// Run by `bun run typecheck`: every value in gen/format-cases.json (written by `cargo test -p culprit-core` from
// culprit_core::format) must read the same here, so a metric in the app says what the finding's prose and brief say.
import cases from "../gen/format-cases.json";
import { bytes, count, days, duration, percent, percentValue } from "./format";

const formatters: Record<string, (value: number) => string> = { duration, bytes, count, percent, percent_value: percentValue, days };
const failures: string[] = [];
let checked = 0;
for (const [name, list] of Object.entries(cases as unknown as Record<string, [number, string][]>)) {
  const format = formatters[name];
  if (!format) {
    failures.push(`format.ts has no ${name}`);
    continue;
  }
  for (const [value, rust] of list) {
    checked++;
    const app = format(value);
    if (app !== rust) failures.push(`${name}(${value}): format.ts says "${app}", culprit_core::format "${rust}"`);
  }
}
if (failures.length > 0) {
  throw new Error(`format.ts reads numbers differently from culprit_core::format:\n${failures.join("\n")}`);
}
if ([null, undefined, NaN, Infinity].some((value) => percentValue(value) !== "—")) throw new Error("Missing percentages must stay unavailable");
console.log(`format.ts reads all ${checked} shared cases as culprit_core::format does`);
