//! Drives an ORM-shaped workload against the synthetic `culprit_demo` shop (dev/mssql/setup.sh) so Query Store has
//! realistic history: parameterized RPC calls the way a Node.js or .NET driver sends them, with strings as nvarchar.
//! It plants an N+1 loop, a varchar column searched with nvarchar parameters, scans that want missing indexes, and a
//! parameter-sensitive query whose cached plan suits only rare values.
//!
//! CULPRIT_MSSQL_ADO='Server=127.0.0.1,14333;Database=culprit_demo;User ID=sa;Password=…;Encrypt=false;TrustServerCertificate=true' \
//!   cargo run --release -p culprit-mssql --example mssql_workload

use std::time::Instant;

use tiberius::{Client, Config, Query};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type Db = Client<Compat<TcpStream>>;

async fn connect(ado: &str) -> Db {
    let config = Config::from_ado_string(ado).expect("valid ADO string");
    let tcp = TcpStream::connect(config.get_addr()).await.expect("tcp connect");
    tcp.set_nodelay(true).expect("nodelay");
    Client::connect(config, tcp.compat_write()).await.expect("login")
}

/// Setup reads carry Culprit's own marker so scans leave them out of the workload.
async fn column<T: Copy + for<'a> tiberius::FromSql<'a>>(db: &mut Db, sql: &str) -> Vec<T> {
    let rows = db.simple_query(sql).await.unwrap().into_first_result().await.unwrap();
    rows.iter().map(|r| r.get::<T, _>(0).unwrap()).collect()
}

async fn strings(db: &mut Db, sql: &str) -> Vec<String> {
    let rows = db.simple_query(sql).await.unwrap().into_first_result().await.unwrap();
    rows.iter().map(|r| r.get::<&str, _>(0).unwrap().to_string()).collect()
}

async fn step(db: &mut Db, label: &str, times: usize, mut make: impl FnMut(usize) -> Query<'static>) {
    let started = Instant::now();
    for i in 0..times {
        make(i).query(db).await.unwrap().into_results().await.unwrap();
    }
    eprintln!("{label:<48} {times:>6} calls  {:>7.1} s", started.elapsed().as_secs_f64());
}

#[tokio::main]
async fn main() {
    let ado = std::env::var("CULPRIT_MSSQL_ADO").expect("set CULPRIT_MSSQL_ADO");
    let mut db = connect(&ado).await;

    let customers: Vec<i32> =
        column(&mut db, "SELECT /* culprit */ TOP (2000) id FROM shop.customers ORDER BY NEWID()").await;
    let products: Vec<i32> =
        column(&mut db, "SELECT /* culprit */ TOP (5000) id FROM shop.products ORDER BY NEWID()").await;
    let orders: Vec<i64> = column(&mut db, "SELECT /* culprit */ TOP (300) id FROM shop.orders ORDER BY NEWID()").await;
    let emails = strings(&mut db, "SELECT /* culprit */ TOP (600) email FROM shop.customers ORDER BY NEWID()").await;

    step(&mut db, "recent orders per customer (well indexed)", 2_000, |i| {
        let mut q = Query::new(
            "SELECT TOP (20) [id], [status], [total], [created_at] FROM [shop].[orders] WHERE [customer_id] = @P1 ORDER BY [created_at] DESC",
        );
        q.bind(customers[i % customers.len()]);
        q
    })
    .await;

    step(&mut db, "product per cart line (N+1)", 30_000, |i| {
        let mut q = Query::new("SELECT [id], [name], [price] FROM [shop].[products] WHERE [id] = @P1");
        q.bind(products[i % products.len()]);
        q
    })
    .await;

    step(&mut db, "customer by email (nvarchar vs varchar)", emails.len(), |i| {
        let mut q = Query::new("SELECT [id], [name], [segment] FROM [shop].[customers] WHERE [email] = @P1");
        q.bind(emails[i].clone());
        q
    })
    .await;

    step(&mut db, "orders shipped in a window (missing index)", 150, |i| {
        let mut q = Query::new(
            "SELECT [id], [total] FROM [shop].[orders] WHERE [shipped_at] >= DATEADD(day, @P1, '2026-01-01') AND [shipped_at] < DATEADD(day, @P2, '2026-01-01')",
        );
        let day = (i % 250) as i32;
        q.bind(day);
        q.bind(day + 1);
        q
    })
    .await;

    step(&mut db, "items of an order (unindexed order_id)", orders.len(), |i| {
        let mut q = Query::new(
            "SELECT [product_id], [quantity], [unit_price] FROM [shop].[order_items] WHERE [order_id] = @P1",
        );
        q.bind(orders[i]);
        q
    })
    .await;

    let by_priority =
        "SELECT [id], [subject], [opened_at], [status] FROM [shop].[support_tickets] WHERE [priority] = @P1";
    step(&mut db, "urgent tickets (rare value, plan gets cached)", 2_000, |_| {
        let mut q = Query::new(by_priority);
        q.bind(5u8);
        q
    })
    .await;
    step(&mut db, "routine tickets (common value, same plan)", 5, |_| {
        let mut q = Query::new(by_priority);
        q.bind(1u8);
        q
    })
    .await;
}
