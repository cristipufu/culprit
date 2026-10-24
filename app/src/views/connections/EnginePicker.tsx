import { Check, ChevronDown, Database, Search } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { EngineBadge } from "../../components/EngineBadge";
import type { Engine } from "../../gen/Engine";
import { connectFacts, ENGINES } from "../../lib/engines";

/** A bounded, searchable provider menu. The engine catalog owns its options and labels. */
export function EnginePicker({ id, value, onChange, autoFocus }: { id: string; value: Engine | null; onChange: (engine: Engine) => void; autoFocus: boolean }) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const listId = useId();
  const options = ENGINES.filter((engine) => connectFacts(engine).choice.toLowerCase().includes(query.trim().toLowerCase()));
  const current = Math.min(active, Math.max(0, options.length - 1));
  const optionId = (engine: Engine) => `${listId}-${engine}`;

  useEffect(() => {
    if (!open) return;
    const frame = requestAnimationFrame(() => search.current?.focus());
    const outside = (event: PointerEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    window.addEventListener("pointerdown", outside);
    return () => {
      cancelAnimationFrame(frame);
      window.removeEventListener("pointerdown", outside);
    };
  }, [open]);

  useEffect(() => {
    if (open && options[current]) document.getElementById(optionId(options[current]))?.scrollIntoView({ block: "nearest" });
  }, [open, current, query]);

  const show = (last = false) => {
    setQuery("");
    setActive(Math.max(0, value ? ENGINES.indexOf(value) : last ? ENGINES.length - 1 : 0));
    setOpen(true);
  };
  const close = () => {
    setOpen(false);
    trigger.current?.focus();
  };
  const choose = (engine: Engine) => {
    onChange(engine);
    close();
  };

  return (
    <div className="engine-picker" ref={root} onBlur={(event) => {
      if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setOpen(false);
    }}>
      <button id={id} ref={trigger} type="button" className="engine-trigger" autoFocus={autoFocus}
        aria-haspopup="listbox" aria-expanded={open} aria-controls={open ? listId : undefined}
        onClick={() => open ? close() : show()}
        onKeyDown={(event) => {
          if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            show(event.key === "ArrowUp");
          }
        }}>
        {value ? <EngineBadge engine={value} /> : <Database size={18} aria-hidden="true" />}
        <span>{value ? connectFacts(value).choice : "Select database"}</span>
        <ChevronDown size={14} aria-hidden="true" />
      </button>
      {open && (
        <div className="engine-picker-menu" onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            close();
          } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            setActive(options.length ? (current + (event.key === "ArrowDown" ? 1 : -1) + options.length) % options.length : 0);
          } else if (event.key === "Enter") {
            event.preventDefault();
            if (options[current]) choose(options[current]);
          }
        }}>
          <div className="engine-search">
            <Search size={14} aria-hidden="true" />
            <input ref={search} role="combobox" aria-label="Search database types" aria-expanded="true"
              aria-controls={listId} aria-autocomplete="list" aria-activedescendant={options[current] ? optionId(options[current]) : undefined}
              value={query} placeholder="Search databases…" autoComplete="off" spellCheck={false}
              onPaste={(event) => event.stopPropagation()}
              onChange={(event) => { setQuery(event.target.value); setActive(0); }} />
          </div>
          <div className="engine-options" id={listId} role="listbox" aria-label="Database types">
            {options.map((engine, index) => (
              <div key={engine} id={optionId(engine)} role="option" aria-selected={value === engine}
                className={`engine-option ${index === current ? "active" : ""}`}
                onPointerMove={() => setActive(index)} onMouseDown={(event) => event.preventDefault()} onClick={() => choose(engine)}>
                <EngineBadge engine={engine} />
                <span>{connectFacts(engine).choice}</span>
                {value === engine && <Check size={14} aria-hidden="true" />}
              </div>
            ))}
            {options.length === 0 && <p className="engine-empty" role="status">No matching database</p>}
          </div>
        </div>
      )}
    </div>
  );
}
