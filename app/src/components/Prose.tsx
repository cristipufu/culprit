/** Plain text with `inline code` spans, as the checks write it. Never renders HTML from the database. */
export function Prose({ text }: { text: string }) {
  const parts = text.split("`");
  return (
    <>
      {parts.map((part, i) =>
        i % 2 === 1 ? (
          <code key={i} className="inline-code">
            {part}
          </code>
        ) : (
          part
        ),
      )}
    </>
  );
}
