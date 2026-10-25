use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "culprit-ssh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ssh(destination: &str) -> SshConfig {
    SshConfig { destination: destination.into(), port: None, identity_file: None }
}
fn spec() -> ConnectionSpec {
    ConnectionSpec {
        engine: Some(Engine::Postgres),
        host: "shop.internal.example".into(),
        ssh: Some(ssh("prod-bastion")),
        ..Default::default()
    }
}

#[test]
fn a_retired_ssh_session_cannot_replace_the_latest_attempts_diagnosis() {
    let failures = Arc::new(Mutex::new(HashMap::new()));
    let target = Target::new("db.example", 5432).unwrap();
    let old = begin(&failures, &target);
    let current = begin(&failures, &target);
    record(&failures, &target, &old, command::failure(b"Host key verification failed"));
    assert!(failures.lock().unwrap()[&target].error.is_none());
    record(&failures, &target, &current, command::failure(b"Permission denied"));
    assert_eq!(failures.lock().unwrap()[&target].error.as_ref().unwrap().message, "SSH authentication failed");
}

fn fixture(mode: &'static str, marker: Option<PathBuf>) -> Factory {
    Arc::new(move |_, _| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "ssh::tests::child_fixture", "--nocapture"]).env("CULPRIT_SSH_TEST_CHILD", mode);
        if let Some(marker) = &marker {
            command.env("CULPRIT_SSH_TEST_MARKER", marker);
        }
        Ok(command)
    })
}
fn manager(mode: &'static str, marker: Option<PathBuf>) -> Manager {
    Manager { factory: fixture(mode, marker), ..Manager::default() }
}

#[test]
fn child_fixture() {
    use std::io::{Read, Write};
    let Ok(mode) = std::env::var("CULPRIT_SSH_TEST_CHILD") else { return };
    match mode.as_str() {
        "hostkey" => {
            eprintln!("Host key verification failed. private-proxy-diagnostic");
        }
        "large" => {
            let _ = std::io::stderr().write_all(&vec![b'x'; MAX_STDERR + 1]);
        }
        "linger" => {
            let marker = PathBuf::from(std::env::var("CULPRIT_SSH_TEST_MARKER").unwrap());
            std::fs::write(marker.with_extension("ready"), "ready").unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            std::fs::write(marker, "orphaned").unwrap();
        }
        "wrapper" => {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", "ssh::tests::child_fixture", "--nocapture"])
                .env("CULPRIT_SSH_TEST_CHILD", "linger")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = child.spawn().unwrap().wait();
        }
        "echo" => {
            let mut bytes = [0; 4];
            std::io::stdin().read_exact(&mut bytes).unwrap();
            std::io::stdout().write_all(&bytes).unwrap();
        }
        "wait" => {
            std::io::stdout().write_all(b"SSH_READY").unwrap();
            let mut bytes = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut bytes);
        }
        _ => panic!("Unknown child fixture"),
    }
}

#[test]
fn destination_parsing_preserves_aliases_users_ipv6_and_explicit_ports() {
    for (input, destination, port) in [
        ("prod-bastion", "prod-bastion", None),
        ("ec2-user@bastion.example:2222", "ec2-user@bastion.example", Some(2222)),
        ("ssh://reader@bastion:22", "reader@bastion", Some(22)),
        ("reader@[::1]:2222", "reader@::1", Some(2222)),
        ("[::1]", "::1", None),
        ("reader@[2001:db8::1]:2222", "reader@2001:db8::1", Some(2222)),
        ("user@example.com@bastion", "user@example.com@bastion", None),
        ("::1", "::1", None),
    ] {
        let normalized = normalize(ssh(input), Engine::Postgres).unwrap();
        assert_eq!((normalized.destination.as_str(), normalized.port), (destination, port));
    }
    let duplicate = SshConfig { port: Some(23), ..ssh("bastion:22") };
    assert!(normalize(duplicate, Engine::Postgres).is_err());
    assert!(normalize(ssh("bastion"), Engine::Sqlite).is_err());
}

#[test]
fn commands_and_proxy_expansion_metacharacters_are_rejected_before_persistence() {
    for input in [
        "",
        "-oProxyCommand=bad",
        "host -oProxyCommand=bad",
        "$(touch bad)",
        "host;bad",
        "u'@host",
        "user\n@host",
        "user:password@host",
        "ssh://host/path",
        "host:0",
        "host:65536",
        "u`bad`@host",
        "host%h",
        "[bastion.example.com]",
        "[127.0.0.1]",
        "bástion.example.com",
    ] {
        assert!(normalize(ssh(input), Engine::Postgres).is_err(), "accepted {input:?}");
    }
    for host in ["host;bad", "host bad", "-host", "host\n", "%h", "[bastion]", "[127.0.0.1]", "bástion"] {
        assert!(Target::new(host, 5432).is_err());
    }
    assert_eq!(Target::new("::1", 5432).unwrap().forwarding(), "[::1]:5432");
}

#[test]
fn ipv6_destinations_use_openssh_host_syntax_and_database_hosts_are_normalized() {
    let ssh = normalize(ssh("reader@[2001:db8::1]:2222"), Engine::Postgres).unwrap();
    let command = command::configure(Command::new("ssh"), &ssh, &Target::new("[::1]", 5432).unwrap());
    let args: Vec<_> = command.as_std().get_args().map(|arg| arg.to_str().unwrap()).collect();
    assert!(args.ends_with(&["-W", "[::1]:5432", "--", "reader@2001:db8::1"]));
    let mut spec = ConnectionSpec { host: " [::1] ".into(), ..spec() };
    validate_spec(&mut spec).unwrap();
    assert_eq!(spec.host, "::1");
    let ssh = SshConfig { identity_file: Some("~/keys/%h.key".into()), ..ssh };
    assert!(normalize(ssh, Engine::Postgres).unwrap_err().message.contains("'%'"));
}

#[test]
fn arguments_keep_a_key_path_as_one_argument_and_disable_prompts_and_background_masters() {
    let ssh = SshConfig {
        port: Some(2222),
        identity_file: Some("key with spaces & symbols".into()),
        ..ssh("reader@bastion")
    };
    let command = command::configure(Command::new("ssh"), &ssh, &Target::new("db.example", 5432).unwrap());
    let args: Vec<_> = command.as_std().get_args().map(|arg| arg.to_str().unwrap()).collect();
    for required in [
        "-oBatchMode=yes",
        "-oStrictHostKeyChecking=yes",
        "-oClearAllForwardings=yes",
        "-oForkAfterAuthentication=no",
        "-oControlMaster=no",
        "-oPermitLocalCommand=no",
        "-a",
        "-x",
    ] {
        assert!(args.contains(&required));
    }
    assert!(args.windows(2).any(|args| args == ["-i", "key with spaces & symbols"]));
    assert!(args.ends_with(&["-W", "db.example:5432", "--", "reader@bastion"]));
    assert!(!args.iter().any(|arg| arg.contains("ControlPath=")), "An existing user-owned SSH master can be reused");
}

#[tokio::test]
async fn routes_are_shared_only_while_a_provider_owns_them_and_keep_logical_endpoints() {
    let manager = manager("echo", None);
    let (mut first, mut second) = (spec(), spec());
    manager.attach(&mut first).unwrap();
    manager.attach(&mut second).unwrap();
    assert_eq!(first.host, "shop.internal.example");
    assert!(first.port.is_none());
    assert!(Arc::ptr_eq(first.transport.as_ref().unwrap(), second.transport.as_ref().unwrap()));
    let endpoint = first.transport.as_ref().unwrap().endpoint(&first.host, 5432).await.unwrap();
    assert!(endpoint.ip().is_loopback());
    let mut socket = TcpStream::connect(endpoint).await.unwrap();
    socket.write_all(b"ping").await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut response)).await.unwrap().unwrap();
    assert!(response.windows(4).any(|bytes| bytes == b"ping"));
    drop(first);
    drop(second);
    tokio::task::yield_now().await;
    assert!(TcpStream::connect(endpoint).await.is_err(), "The forwarding port survived its last owner");
}

#[tokio::test]
async fn ssh_errors_are_available_before_tcp_eof_and_never_contain_raw_diagnostics() {
    let manager = manager("hostkey", None);
    let mut spec = spec();
    manager.attach(&mut spec).unwrap();
    let route = spec.transport.as_ref().unwrap();
    let endpoint = route.endpoint(&spec.host, 5432).await.unwrap();
    let mut socket = TcpStream::connect(endpoint).await.unwrap();
    let mut ignored = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut ignored)).await.unwrap().unwrap();
    let error = route.failure(&spec.host, 5432).expect("SSH diagnosis was lost before driver EOF");
    assert_eq!(error.message, "SSH host key verification failed");
    assert!(!serde_json::to_string(&error).unwrap().contains("private-proxy-diagnostic"));
}

#[tokio::test]
async fn output_and_session_counts_are_bounded_without_waiting_tasks() {
    assert!(diagnostics(&vec![b'x'; MAX_STDERR + 1][..]).await.is_err());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let manager = Manager {
        sessions: Arc::new(Semaphore::new(0)),
        factory: Arc::new(move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(Command::new("unused"))
        }),
        ..Manager::default()
    };
    let mut spec = spec();
    manager.attach(&mut spec).unwrap();
    let route = spec.transport.as_ref().unwrap();
    let endpoint = route.endpoint(&spec.host, 5432).await.unwrap();
    let mut socket = TcpStream::connect(endpoint).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut response)).await.unwrap().unwrap();
    assert!(route.failure(&spec.host, 5432).unwrap().message.contains("Too many SSH"));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "A rejected session launched SSH work");
}

async fn check_cancellation(drop_provider: bool) {
    let temp = Temp::new();
    let marker = temp.0.join("orphan");
    let manager = manager("wrapper", Some(marker.clone()));
    let mut spec = spec();
    manager.attach(&mut spec).unwrap();
    let endpoint = spec.transport.as_ref().unwrap().endpoint(&spec.host, 5432).await.unwrap();
    let owner = Arc::downgrade(spec.transport.as_ref().unwrap());
    let mut socket = TcpStream::connect(endpoint).await.unwrap();
    for _ in 0..500 {
        if marker.with_extension("ready").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(marker.with_extension("ready").exists(), "The proxy fixture did not start");
    if drop_provider {
        drop(spec);
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut response)).await.unwrap().unwrap();
    } else {
        drop(socket);
    }
    tokio::time::sleep(Duration::from_millis(1700)).await;
    assert!(!marker.exists(), "An SSH ProxyCommand descendant survived its owner");
    if drop_provider {
        assert!(TcpStream::connect(endpoint).await.is_err());
    } else {
        assert!(owner.upgrade().is_some(), "The provider should retain its reusable forwarding listener");
    }
    assert_eq!(manager.sessions.available_permits(), MAX_SESSIONS);
}

#[tokio::test]
async fn dropping_the_last_provider_kills_proxy_descendants_and_releases_the_port() {
    check_cancellation(true).await;
}

#[tokio::test]
async fn canceling_a_driver_session_kills_ssh_while_the_provider_is_retained() {
    check_cancellation(false).await;
}

#[tokio::test]
async fn redirect_targets_are_capped_and_the_database_identity_remains_unchanged() {
    let manager = manager("echo", None);
    let mut spec = spec();
    manager.attach(&mut spec).unwrap();
    let route = spec.transport.as_ref().unwrap();
    for n in 0..MAX_TARGETS {
        route.endpoint(&format!("worker-{n}.database.windows.net"), 11001).await.unwrap();
    }
    assert!(route.endpoint("another.database.windows.net", 11001).await.is_err());
    assert_eq!(spec.host, "shop.internal.example");
}

#[test]
fn editing_ssh_preserves_cloud_authentication_and_credentials_and_failed_edits_do_not_persist() {
    let temp = Temp::new();
    let ws = crate::Workspace::open_in(&temp.0);
    let saved = ws
        .add_connection(crate::NewConnection {
            name: "cloud".into(),
            input: "postgres://app@shop.abc.us-east-1.rds.amazonaws.com/shop".into(),
            auth: Some(culprit_core::AuthKind::AwsIam),
            ..Default::default()
        })
        .unwrap();
    let changed = ws.set_ssh("cloud", Some(ssh("reader@bastion:2222"))).unwrap();
    assert_eq!(changed.host, saved.host);
    assert_eq!(changed.auth, saved.auth);
    assert_eq!(changed.database, saved.database);
    assert_eq!(changed.ssh.unwrap().port, Some(2222));
    let before = std::fs::read(temp.0.join("connections.toml")).unwrap();
    assert!(ws.set_ssh("cloud", Some(ssh("host;bad"))).is_err());
    assert_eq!(std::fs::read(temp.0.join("connections.toml")).unwrap(), before);
    assert!(ws.set_ssh("cloud", None).unwrap().ssh.is_none());
}

#[tokio::test]
async fn testing_a_draft_uses_saved_credentials_and_preserves_the_saved_route() {
    let temp = Temp::new();
    let mut ws = crate::Workspace::open_in(&temp.0);
    ws.ssh = manager("echo", None);
    ws.add_connection(crate::NewConnection {
        name: "draft".into(),
        input: "postgres://reader@db.internal/shop".into(),
        password: Some("private-db-password".into()),
        ..Default::default()
    })
    .unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let observed = seen.clone();
    let ws = ws.with_provider(
        Engine::Postgres,
        Arc::new(move |spec| {
            assert_eq!(spec.host, "db.internal");
            assert_eq!(spec.password.as_deref(), Some("private-db-password"));
            assert_eq!(spec.ssh.as_ref().unwrap().destination, "test-bastion");
            assert!(spec.transport.is_some());
            observed.fetch_add(1, Ordering::SeqCst);
            Err(Error::new(ErrorCode::ConnectionFailed, "test provider refused the probe"))
        }),
    );
    let before = std::fs::read(temp.0.join("connections.toml")).unwrap();
    assert!(ws.test_ssh("draft", Some(ssh("test-bastion"))).await.is_err());
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(temp.0.join("connections.toml")).unwrap(), before);
    assert!(ws.connection("draft").unwrap().ssh.is_none());
}

/// An isolated OpenSSH server and PostgreSQL with a trusted, DNS-only `localhost` certificate are required.
/// The test uses its own SSH configuration and known_hosts; it never changes the user's SSH files.
#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "requires CULPRIT_SSH_LIVE_CONFIG, CULPRIT_SSH_LIVE_PG_PORT and CULPRIT_SSH_LIVE_CA"]
async fn real_openssh_preserves_strict_tls_identity_and_rejects_a_mismatched_hostname() {
    let config = std::env::var("CULPRIT_SSH_LIVE_CONFIG").unwrap();
    let port = std::env::var("CULPRIT_SSH_LIVE_PG_PORT").unwrap();
    let ca = std::env::var("CULPRIT_SSH_LIVE_CA").unwrap();
    let temp = Temp::new();
    let mut ws = crate::Workspace::open_in(&temp.0);
    ws.ssh.factory = Arc::new(move |ssh, target| {
        let mut command = crate::program::command(crate::program::Cli::Ssh)?;
        command.args(["-F", &config]);
        Ok(command::configure(command, ssh, target))
    });
    for (name, host) in [("valid", "localhost"), ("mismatch", "127.0.0.1")] {
        ws.add_connection(crate::NewConnection {
            name: name.into(),
            input: format!("postgres://ssh_fixture@{host}:{port}/ssh_fixture?sslmode=verify-full&sslrootcert={ca}"),
            password: Some("fixture-only-password".into()),
            ssh: Some(ssh("fixture-bastion")),
            ..Default::default()
        })
        .unwrap();
    }
    let server = ws.probe("valid").await.unwrap();
    assert_eq!(server.database, "ssh_fixture");
    let result = ws.run_read("valid", "SELECT 42 AS answer", 10).await.unwrap();
    assert_eq!(result.rows[0][0], serde_json::json!(42));
    let error = ws.probe("mismatch").await.expect_err("The certificate only covers localhost, never its IP");
    assert!(error.message.to_ascii_lowercase().contains("certificate"), "{error:?}");
    assert_eq!(ws.connection("valid").unwrap().host, "localhost");
}
