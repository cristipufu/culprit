//! Application-shaped load for the `shop` demo database (dev/mysql). Statements are sent with literal values, the way
//! most MySQL drivers interpolate parameters client-side, so performance_schema keeps plannable samples.
//!
//! CULPRIT_MYSQL_URL='mysql://root:…@127.0.0.1:33306/shop' cargo run -p culprit-mysql --example mysql_workload

use std::time::Instant;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, Pool};

type Run = std::result::Result<(), mysql_async::Error>;

async fn n_plus_one_product_lookups(mut conn: Conn) -> Run {
    for i in 0..30_000u32 {
        let id = 1 + (i * 7919) % 20_000;
        conn.query_drop(format!("SELECT id, name, price FROM products WHERE id = {id}")).await?;
    }
    Ok(())
}

async fn promo_code_scans(mut conn: Conn) -> Run {
    for i in 0..60u32 {
        let code = format!("SPRING{}", i % 10);
        conn.query_drop(format!(
            "SELECT o.id, o.customer_id, o.total FROM orders o WHERE o.promo_code = '{code}' ORDER BY o.total DESC LIMIT 50"
        ))
        .await?;
    }
    Ok(())
}

async fn top_spenders(mut conn: Conn) -> Run {
    for _ in 0..20u32 {
        conn.query_drop(
            "SELECT customer_id, channel, SUM(total) AS spent, COUNT(*) AS orders FROM orders GROUP BY customer_id, channel ORDER BY spent DESC LIMIT 20",
        )
        .await?;
    }
    Ok(())
}

async fn healthy_lookups(mut conn: Conn) -> Run {
    for i in 0..4_000u64 {
        let customer = 1 + (i * 104_729) % 200_000;
        conn.query_drop(format!(
            "SELECT id, status, total, created_at FROM orders WHERE customer_id = {customer} ORDER BY created_at DESC LIMIT 20"
        ))
        .await?;
        let order = 1 + (i * 15_485_863) % 500_000;
        conn.query_drop(format!(
            "SELECT oi.quantity, oi.price, p.name FROM order_items oi JOIN products p ON p.id = oi.product_id WHERE oi.order_id = {order}"
        ))
        .await?;
        if i % 2 == 0 {
            let day = 1 + i % 28;
            conn.query_drop(format!(
                "SELECT COUNT(*) FROM orders WHERE status = 'shipped' AND created_at >= '2026-09-{day:02}'"
            ))
            .await?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("CULPRIT_MYSQL_URL").map_err(|_| "set CULPRIT_MYSQL_URL")?;
    let pool = Pool::new(Opts::from_url(&url)?);
    let started = Instant::now();
    let (a, b, c, d) = tokio::join!(
        async { n_plus_one_product_lookups(pool.get_conn().await?).await },
        async { promo_code_scans(pool.get_conn().await?).await },
        async { top_spenders(pool.get_conn().await?).await },
        async { healthy_lookups(pool.get_conn().await?).await },
    );
    a?;
    b?;
    c?;
    d?;
    pool.disconnect().await?;
    println!("workload finished in {:.1} s", started.elapsed().as_secs_f64());
    Ok(())
}
