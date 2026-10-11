export function Logo({ size = 22 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 32 32" aria-hidden="true">
      <rect width="32" height="32" rx="8" fill="var(--brand)" />
      <circle cx="14" cy="14" r="6.5" fill="none" stroke="var(--brand-ink)" strokeWidth="2.6" />
      <path d="M19 19l5.5 5.5" stroke="var(--brand-ink)" strokeWidth="2.8" strokeLinecap="round" />
      <circle cx="14" cy="14" r="2" fill="var(--brand-ink)" />
    </svg>
  );
}
