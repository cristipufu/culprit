use super::*;
use std::{collections::VecDeque, sync::Mutex};

#[derive(Default)]
struct Fake {
    aws: Mutex<VecDeque<Result<Value>>>,
    graph: Mutex<VecDeque<Result<Value>>>,
    arm: Mutex<VecDeque<Result<Value>>>,
    aws_calls: Mutex<Vec<Vec<String>>>,
    graph_calls: Mutex<Vec<Option<String>>>,
    arm_calls: Mutex<Vec<String>>,
}
impl Transport for Fake {
    async fn aws(&self, args: &[String]) -> Result<Value> {
        self.aws_calls.lock().unwrap().push(args.to_vec());
        self.aws.lock().unwrap().pop_front().expect("Unexpected AWS request")
    }
    async fn graph(&self, _query: &str, token: Option<&str>) -> Result<Value> {
        self.graph_calls.lock().unwrap().push(token.map(str::to_string));
        self.graph.lock().unwrap().pop_front().unwrap_or_else(|| Ok(json!({"data":[]})))
    }
    async fn token(&self) -> Result<String> {
        Ok("test-bearer".into())
    }
    async fn arm(&self, url: &str, token: &str) -> Result<Value> {
        assert_eq!(token, "test-bearer");
        self.arm_calls.lock().unwrap().push(url.into());
        self.arm.lock().unwrap().pop_front().expect("Unexpected ARM request")
    }
}
fn identity() -> Result<Value> {
    Ok(json!({"Account":"123456789012"}))
}
fn instance(i: usize) -> Value {
    json!({"DBInstanceIdentifier":format!("server-{i}"),"Engine":"postgres","DBName":format!("db-{i}"),
        "Endpoint":{"Address":format!("host-{i}.rds.amazonaws.com"),"Port":5432}})
}
fn sql(i: usize) -> Value {
    json!({"server":format!("server-{i}"),"database":format!("db-{i}"),"location":"west","resourceGroup":"rg","subscriptionId":"s"})
}
fn deadline() -> Instant {
    Instant::now() + SEARCH_TIMEOUT
}
fn region() -> Vec<String> {
    vec!["us-east-1".into()]
}

#[tokio::test]
async fn aws_stops_after_one_bounded_page_when_twenty_matches_are_available() {
    let fake = Fake::default();
    fake.aws.lock().unwrap().extend([
        identity(),
        Ok(json!({"DBInstances":(0..PAGE_SIZE).map(instance).collect::<Vec<_>>(),"NextToken":"later"})),
    ]);
    let result = aws_with(&fake, &region(), &[], deadline()).await.unwrap();
    assert_eq!(result.items.len(), DISCOVERY_LIMIT);
    assert!(result.has_more && result.warnings.is_empty());
    let calls = fake.aws_calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "Remaining instances, clusters and regions must not be fetched");
    assert!(calls[1].windows(2).any(|p| p == ["--max-items", "50"]));
    assert!(calls[1].windows(2).any(|p| p == ["--page-size", "50"]));
}

#[tokio::test]
async fn aws_search_can_find_a_match_in_the_ten_thousandth_record() {
    let fake = Fake::default();
    fake.aws.lock().unwrap().push_back(identity());
    for page in 0..200 {
        let mut value = json!({"DBInstances":(page*PAGE_SIZE..(page+1)*PAGE_SIZE).map(instance).collect::<Vec<_>>()});
        if page < 199 {
            value["NextToken"] = format!("page-{}", page + 1).into();
        }
        fake.aws.lock().unwrap().push_back(Ok(value));
    }
    fake.aws.lock().unwrap().push_back(Ok(json!({"DBClusters":[]})));
    let result = aws_with(&fake, &region(), &["db-9999".into()], deadline()).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].database, "db-9999");
    assert!(!result.has_more && result.warnings.is_empty());
    let calls = fake.aws_calls.lock().unwrap();
    assert_eq!(calls.len(), 202);
    assert!(calls[2].iter().any(|s| s == "--starting-token=page-1"));
}

#[tokio::test]
async fn repeated_tokens_preserve_partial_results_instead_of_looping() {
    let fake = Fake::default();
    fake.aws.lock().unwrap().extend([
        identity(),
        Ok(json!({"DBInstances":[instance(1)],"NextToken":"same"})),
        Ok(json!({"DBInstances":[instance(1)],"NextToken":"same"})),
        Ok(json!({"DBClusters":[]})),
    ]);
    let result = aws_with(&fake, &region(), &[], deadline()).await.unwrap();
    assert_eq!(result.items.len(), 1, "Resources must be deduplicated");
    assert!(result.has_more && result.warnings[0].contains("repeated"));
}

#[tokio::test]
async fn azure_follows_graph_tokens_and_stops_without_loading_flexible_servers() {
    let fake = Fake::default();
    fake.graph.lock().unwrap().extend([
        Ok(json!({"data":[sql(999)],"$skipToken":"sql-page-2"})),
        Ok(json!({"data":(0..PAGE_SIZE).map(sql).collect::<Vec<_>>()})),
    ]);
    let result = azure_with(&fake, &[], deadline()).await.unwrap();
    assert_eq!(result.items.len(), DISCOVERY_LIMIT);
    assert!(result.has_more && result.warnings.is_empty());
    assert_eq!(*fake.graph_calls.lock().unwrap(), vec![None, Some("sql-page-2".into())]);
    assert!(fake.arm_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_azure_continuation_is_reported_and_preserves_collected_rows() {
    let fake = Fake::default();
    fake.graph.lock().unwrap().push_back(Ok(json!({"data":[sql(1)],"resultTruncated":"true"})));
    let result = azure_with(&fake, &[], deadline()).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert!(result.has_more && result.warnings[0].contains("continuation"));
}

const SERVER: &str = "/subscriptions/s/resourceGroups/rg/providers/Microsoft.DBforPostgreSQL/flexibleServers/pg-demo";
fn flexible() -> Value {
    json!({"id":SERVER,"name":"pg-demo","type":"microsoft.dbforpostgresql/flexibleservers",
        "location":"west","resourceGroup":"rg","subscriptionId":"s","entra":"Enabled","state":"Ready"})
}
#[tokio::test]
async fn flexible_server_and_database_pages_reach_later_matches() {
    let fake = Fake::default();
    let next = format!("{MANAGEMENT}{SERVER}/databases?api-version=2024-08-01&$skiptoken=second");
    fake.graph.lock().unwrap().extend([
        Ok(json!({"data":[]})),
        Ok(json!({"data":[],"$skipToken":"servers-2"})),
        Ok(json!({"data":[flexible()]})),
    ]);
    fake.arm.lock().unwrap().extend([
        Ok(json!({"value":[{"name":"postgres"},{"name":"other"}],"nextLink":next})),
        Ok(json!({"value":[{"name":"needle"}]})),
    ]);
    let result = azure_with(&fake, &["needle".into()], deadline()).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].database, "needle");
    assert_eq!(result.items[0].auth, AuthKind::Entra);
    assert!(!result.has_more);
    assert_eq!(fake.graph_calls.lock().unwrap().len(), 3);
    assert_eq!(fake.arm_calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_foreign_continuation_never_receives_the_bearer_token() {
    let fake = Fake::default();
    fake.graph.lock().unwrap().extend([Ok(json!({"data":[]})), Ok(json!({"data":[flexible()]}))]);
    fake.arm
        .lock()
        .unwrap()
        .push_back(Ok(json!({"value":[{"name":"found"}],"nextLink":"https://attacker.invalid/databases"})));
    let result = azure_with(&fake, &[], deadline()).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert!(result.has_more && !result.warnings.is_empty());
    assert_eq!(fake.arm_calls.lock().unwrap().len(), 1);
}

#[test]
fn continuation_validation_rejects_other_collections_and_authorities() {
    let original = format!("{MANAGEMENT}{SERVER}/databases?api-version=2024-08-01");
    for next in [
        "http://management.azure.com/x",
        "https://management.azure.com.evil.invalid/x",
        "https://user@management.azure.com/x",
        "https://management.azure.com:443/x",
        "https://management.azure.com/other/databases",
        "//attacker.invalid/x",
    ] {
        assert!(arm_next(&original, next.into()).is_err(), "{next}");
    }
    assert!(arm_next(&original, format!("{SERVER}/databases?$skiptoken=a")).is_ok());
    assert!(Cursor::default().advance(Some("x".repeat(MAX_CURSOR + 1))).is_err());
}

#[tokio::test]
async fn exhausted_budgets_launch_no_more_work_and_keep_partial_status() {
    let mut search = Search::new(&[], deadline());
    search.success = true;
    search.requests = MAX_REQUESTS;
    let touched = std::sync::atomic::AtomicBool::new(false);
    assert!(
        search
            .request(async {
                touched.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await
            .is_err()
    );
    assert!(!touched.load(std::sync::atomic::Ordering::SeqCst));
    let result = search.finish().unwrap();
    assert!(result.has_more && !result.warnings.is_empty());
    assert!(Search::new(&[], Instant::now()).request(async { Ok(()) }).await.is_err());
}

#[tokio::test]
async fn failed_sources_do_not_become_an_empty_complete_inventory() {
    let fake = Fake::default();
    fake.aws.lock().unwrap().extend([
        identity(),
        Err(Error::new(ErrorCode::PermissionDenied, "No RDS permission")),
        Err(Error::new(ErrorCode::PermissionDenied, "No cluster permission")),
    ]);
    assert!(aws_with(&fake, &region(), &[], deadline()).await.is_err());
}

#[tokio::test]
async fn an_empty_aws_page_with_a_cursor_does_not_end_the_search() {
    let fake = Fake::default();
    fake.aws.lock().unwrap().extend([
        identity(),
        Ok(json!({"DBInstances":[],"NextToken":"second"})),
        Ok(json!({"DBInstances":[instance(4)]})),
        Ok(json!({"DBClusters":[]})),
    ]);
    let result = aws_with(&fake, &region(), &[], deadline()).await.unwrap();
    assert_eq!(result.items[0].database, "db-4");
    assert!(!result.has_more);
}

#[tokio::test]
async fn known_servers_remain_available_when_their_database_listing_is_denied() {
    let fake = Fake::default();
    fake.graph.lock().unwrap().extend([Ok(json!({"data":[]})), Ok(json!({"data":[flexible()]}))]);
    fake.arm.lock().unwrap().push_back(Err(Error::new(ErrorCode::PermissionDenied, "Cannot list databases")));
    let result = azure_with(&fake, &[], deadline()).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].database, "postgres");
    assert_eq!(result.items[0].auth, AuthKind::Entra);
    assert!(result.has_more && !result.warnings.is_empty());
}

#[test]
fn database_case_is_preserved_and_provider_sized_pages_are_enforced() {
    let server: FlexibleServer = serde_json::from_value(flexible()).unwrap();
    let mut search = Search::new(&[], deadline());
    search.accept([
        flexible_database(&server, Engine::Postgres, "Reports".into(), true),
        flexible_database(&server, Engine::Postgres, "reports".into(), true),
    ]);
    assert_eq!(search.result.items.len(), 2, "Distinct PostgreSQL databases must not collapse");
    assert!(graph_entries(&json!({"data":vec![Value::Null;PAGE_SIZE+1]})).is_err());
    let original = format!("{MANAGEMENT}{SERVER}/databases?api-version=2024-08-01");
    assert!(
        arm_next(&original, format!("{MANAGEMENT}{}/databases?$skiptoken=next", SERVER.to_ascii_lowercase())).is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn an_inflight_request_has_a_deadline() {
    let mut search = Search::new(&[], deadline());
    let error = search
        .request(async {
            tokio::time::sleep(REQUEST_TIMEOUT * 2).await;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
    assert_eq!(search.requests, 1);
}
