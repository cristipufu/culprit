import type { DatabaseProduct } from "../gen/DatabaseProduct";
import { PRODUCTS } from "../lib/engines";
import { EngineBadge } from "./EngineBadge";

/** Formats and sample actions follow the generated core catalog together. */
export function PlanCatalog({ onSample }: { onSample: (product: DatabaseProduct) => void }) {
  return (
    <>
      <div className="formats">
        {PRODUCTS.map((product) => (
          <div className="format-row" key={product.id} data-product={product.id}>
            <b><EngineBadge engine={product.engine} product={product.prefix} />{product.label}</b>
            <span className="muted">{product.plan_format}</span>
          </div>
        ))}
      </div>
      <div className="welcome-samples">
        <span className="muted">See it with a sample comparison:</span>
        {PRODUCTS.map((product) => (
          <button key={product.id} className="btn ghost small" data-product={product.id} onClick={() => onSample(product.id)}>
            <EngineBadge engine={product.engine} product={product.prefix} />
            {product.label}
          </button>
        ))}
      </div>
    </>
  );
}
