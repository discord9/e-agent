use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn jwt(value: serde_json::Value) -> String {
    format!(
        "x.{}.x",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    )
}

#[test]
fn authorize_url_and_callback_keep_secrets_out_of_errors() {
    let pkce = Pkce::new();
    assert_eq!(pkce.verifier.len(), 86);
    assert_eq!(pkce.state.len(), 43);
    let url = authorize_url("http://localhost:1455/auth/callback", &pkce).unwrap();
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        query["scope"],
        "openid profile email offline_access api.connectors.read api.connectors.invoke"
    );
    assert_eq!(query["originator"], "codex_cli_rs");
    let error =
        callback_result("/auth/callback?code=secret-code&state=wrong", "right").unwrap_err();
    assert!(!format!("{error:#}").contains("secret-code"));
}

#[test]
fn jwt_metadata_and_private_atomic_storage_work() {
    let token = jwt(serde_json::json!({"exp": 2_000_000_000i64, ACCOUNT_CLAIM: "account"}));
    assert_eq!(
        jwt_metadata(&token).unwrap().account_id.as_deref(),
        Some("account")
    );
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    let data = AuthFile {
        tokens: Tokens {
            id_token: token,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            account_id: None,
        },
        last_refresh: Utc::now(),
    };
    save_file(&path, &data).unwrap();
    save_file(&path, &data).unwrap();
    assert_eq!(
        serde_json::from_slice::<AuthFile>(&std::fs::read(&path).unwrap())
            .unwrap()
            .tokens
            .refresh_token,
        "refresh"
    );
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path).unwrap().permissions())
            & 0o777,
        0o600
    );
}

#[test]
fn usable_account_falls_back_to_access_token_claim() {
    let tokens = Tokens {
        id_token: jwt(serde_json::json!({"exp": 2_000_000_000i64})),
        access_token: jwt(serde_json::json!({ACCOUNT_CLAIM: "access-account"})),
        refresh_token: "refresh".into(),
        account_id: None,
    };
    assert_eq!(usable_account(&tokens).unwrap(), "access-account");
}

#[test]
fn jwt_metadata_reads_nested_namespace_claim() {
    let token = jwt(serde_json::json!({
        "exp": 2_000_000_000i64,
        ACCOUNT_CLAIM_NAMESPACE: { ACCOUNT_CLAIM_FIELD: "nested-account" }
    }));
    assert_eq!(
        jwt_metadata(&token).unwrap().account_id.as_deref(),
        Some("nested-account")
    );
}

#[test]
fn private_temp_creation_never_truncates_a_collision() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".auth.existing.tmp");
    std::fs::write(&path, "keep-this").unwrap();
    assert_eq!(
        open_private_temp(&path).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "keep-this");
}

#[tokio::test]
async fn refresh_preserves_partial_fields_and_rotates_token() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        let _ = stream.read(&mut bytes).await.unwrap();
        let body = r#"{"access_token":"new-access","refresh_token":"new-refresh"}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    let data = AuthFile {
        tokens: Tokens {
            id_token: "old-id".into(),
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            account_id: Some("account".into()),
        },
        last_refresh: Utc::now() - ChronoDuration::days(9),
    };
    save_file(&path, &data).unwrap();
    let auth = CodexAuth::from_file(path.clone(), data)
        .unwrap()
        .with_token_endpoint(endpoint);
    auth.refresh_if_needed(false, None).await.unwrap();
    let saved: AuthFile = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved.tokens.id_token, "old-id");
    assert_eq!(saved.tokens.access_token, "new-access");
    assert_eq!(saved.tokens.refresh_token, "new-refresh");
}

#[test]
fn logout_is_idempotent_without_environment_changes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    std::fs::write(&path, "secret").unwrap();
    logout_at(&path).unwrap();
    logout_at(&path).unwrap();
    assert!(!path.exists());
}

#[test]
fn account_id_is_required_and_stored_id_wins() {
    let no_account = Tokens {
        id_token: "not-a-jwt".into(),
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        account_id: None,
    };
    assert!(usable_account(&no_account).is_err());
    let jwt_account = jwt(serde_json::json!({ACCOUNT_CLAIM: "from-jwt"}));
    let stored = Tokens {
        id_token: jwt_account,
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        account_id: Some("stored".into()),
    };
    assert_eq!(usable_account(&stored).unwrap(), "stored");
}

#[tokio::test]
async fn fragmented_callback_reads_complete_get_request_without_secret_leakage() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_callback(listener, "right"));
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /auth/callback?code=secret-code&sta")
        .await
        .unwrap();
    stream
        .write_all(b"te=right HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let callback = receiver.await.unwrap().unwrap();
    assert_eq!(callback.code, "secret-code");
    drop(callback);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_callback(listener, "right"));
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /wrong?code=secret-code&state=right HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let error = receiver.await.unwrap().unwrap_err();
    assert!(!format!("{error:#}").contains("secret-code"));
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 400"));
    assert!(!response.contains("secret-code"));
}

#[tokio::test]
async fn concurrent_forced_refresh_skips_rotated_credentials() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..count]);
        assert!(request.contains("old-refresh"));
        let body = r#"{"access_token":"new-access","refresh_token":"new-refresh"}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    let data = AuthFile {
        tokens: Tokens {
            id_token: "id".into(),
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            account_id: Some("account".into()),
        },
        last_refresh: Utc::now(),
    };
    let auth = CodexAuth::from_file(path, data)
        .unwrap()
        .with_token_endpoint(endpoint);
    let (first, second) = (auth.clone(), auth.clone());
    let (first, second) = tokio::join!(
        first.refresh_after_unauthorized("old-access"),
        second.refresh_after_unauthorized("old-access")
    );
    first.unwrap();
    second.unwrap();
    server.await.unwrap();
    assert_eq!(
        auth.current_access_token_and_account().await.unwrap().0,
        "new-access"
    );
}

#[tokio::test]
async fn permanent_refresh_errors_are_actionable_and_token_safe() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        let body = r#"{"error":"refresh_token_reused","refresh_token":"do-not-leak"}"#;
        stream.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let auth = CodexAuth::test_auth(temp.path().join("auth.json")).with_token_endpoint(endpoint);
    let error = auth.refresh_after_unauthorized("access").await.unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("run `e-agent login`"));
    assert!(!error.contains("do-not-leak"));
}
