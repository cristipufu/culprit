//! Bounded result display. Writes are drained through command completion so truncation cannot hide a late error
//! or report a committed write before PostgreSQL finishes it.
use culprit_core::{
    Result, ResultSet,
    limits::{MAX_RESULT_ROWS, ResultBudget, row_bytes},
};
use futures_util::TryStreamExt;
use tokio_postgres::{Client, SimpleQueryMessage, types::Type};

use crate::{connect::map_error, json_value};

pub const MAX_ROWS: usize = MAX_RESULT_ROWS;

pub async fn collect(
    client: &Client,
    sql: &str,
    columns: Vec<String>,
    types: &[Type],
    limit: usize,
) -> Result<ResultSet> {
    let stream = client.simple_query_raw(sql).await.map_err(map_error)?;
    futures_util::pin_mut!(stream);
    let mut budget = ResultBudget::new(limit);
    let mut rows = Vec::new();
    let mut affected = 0;
    while let Some(message) = stream.try_next().await.map_err(map_error)? {
        match message {
            SimpleQueryMessage::Row(row) => {
                if budget.truncated() {
                    continue;
                }
                let values = (0..columns.len())
                    .map(|i| row.try_get(i))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(map_error)?;
                let size = row_bytes(values.len(), values.iter().map(|v| v.map_or(0, str::len)).sum());
                if !budget.retain(size) {
                    continue;
                }
                rows.push(
                    values
                        .into_iter()
                        .enumerate()
                        .map(|(i, v)| json_value(v.map(str::to_string), types.get(i)))
                        .collect(),
                );
            }
            SimpleQueryMessage::CommandComplete(count) => affected = count,
            _ => {}
        }
    }
    let count = if columns.is_empty() { affected } else { rows.len() as u64 };
    let mut message = format!("{count} row{}", if count == 1 { "" } else { "s" });
    if let Some(notice) = budget.notice() {
        message.push_str(&format!(" ({notice})"));
    }
    Ok(ResultSet { columns, rows, truncated: budget.truncated(), elapsed_ms: 0.0, messages: vec![message] })
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::limits::MAX_RESULT_BYTES as MAX_BYTES;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn message(socket: &mut tokio::io::DuplexStream, tag: u8, body: &[u8]) {
        socket.write_u8(tag).await.unwrap();
        socket.write_u32((body.len() + 4) as u32).await.unwrap();
        socket.write_all(body).await.unwrap();
    }

    // A protocol peer exercises the real streaming driver without needing a live database or executing SQL.
    async fn peer(values: Vec<String>, late_error: bool) -> (Client, tokio::task::JoinHandle<()>) {
        let (client_io, mut socket) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let length = socket.read_u32().await.unwrap() as usize;
            socket.read_exact(&mut vec![0; length - 4]).await.unwrap();
            message(&mut socket, b'R', &0u32.to_be_bytes()).await;
            message(&mut socket, b'Z', b"I").await;
            assert_eq!(socket.read_u8().await.unwrap(), b'Q');
            let length = socket.read_u32().await.unwrap() as usize;
            socket.read_exact(&mut vec![0; length - 4]).await.unwrap();
            let mut description = vec![0, 1, b'v', 0];
            description.extend_from_slice(&[0; 6]); // table and attribute
            description.extend_from_slice(&25u32.to_be_bytes()); // text
            description.extend_from_slice(&[255; 6]); // variable length and type modifier
            description.extend_from_slice(&[0; 2]); // text format
            message(&mut socket, b'T', &description).await;
            let count = values.len();
            for value in values {
                let mut body = vec![0, 1];
                body.extend_from_slice(&(value.len() as u32).to_be_bytes());
                body.extend_from_slice(value.as_bytes());
                message(&mut socket, b'D', &body).await;
            }
            if late_error {
                message(&mut socket, b'E', b"SERROR\0CXX000\0Mlate failure\0\0").await;
            } else {
                message(&mut socket, b'C', format!("SELECT {count}\0").as_bytes()).await;
            }
            message(&mut socket, b'Z', b"I").await;
        });
        let (client, connection) = tokio_postgres::Config::new()
            .user("test")
            .ssl_mode(tokio_postgres::config::SslMode::Disable)
            .connect_raw(client_io, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        (client, server)
    }

    #[tokio::test]
    async fn display_limits_do_not_skip_command_completion_or_late_errors() {
        for late_error in [false, true] {
            let (client, server) = peer((0..100).map(|i| i.to_string()).collect(), late_error).await;
            let result = collect(&client, "SELECT v", vec!["v".into()], &[Type::INT8], 1).await;
            if late_error {
                assert!(result.unwrap_err().message.contains("late failure"));
            } else {
                let result = result.unwrap();
                assert_eq!(result.rows, vec![vec![Value::from(0)]]);
                assert!(result.truncated);
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_single_large_value_cannot_exceed_the_display_budget() {
        let (client, server) = peer(vec!["x".repeat(MAX_BYTES + 1)], false).await;
        let result = collect(&client, "SELECT v", vec!["v".into()], &[Type::TEXT], 100).await.unwrap();
        assert!(result.rows.is_empty() && result.truncated);
        assert!(result.messages[0].contains("8 MiB"));
        server.await.unwrap();
    }
}
