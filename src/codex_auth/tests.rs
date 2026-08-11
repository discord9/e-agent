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
async fn permanent_refresh_errors_are_classified_and_token_safe() {
    // Each permanent failure kind surfaces a distinct, token-safe hint (with
    // no disk recovery available) so users can tell a true expiry/revocation
    // from a cross-process reuse race.
    for (needle, hint) in [
        ("refresh_token_expired", "login has expired"),
        ("refresh_token_reused", "reused by another process"),
        ("refresh_token_invalidated", "login was revoked"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let needle = needle.to_owned();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            let body = format!(r#"{{"error":"{needle}","refresh_token":"do-not-leak"}}"#);
            stream.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let auth =
            CodexAuth::test_auth(temp.path().join("auth.json")).with_token_endpoint(endpoint);
        let error = auth.refresh_after_unauthorized("access").await.unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("run `e-agent login`"), "{error}");
        assert!(error.contains(hint), "{error}");
        assert!(!error.contains("do-not-leak"), "{error}");
    }
}

#[tokio::test]
async fn refresh_reloads_login_replaced_credentials_without_submitting_old_token() {
    // In-memory auth is stale/expired; `e-agent login` replaced the file on
    // disk with fresh tokens. The next refresh boundary must adopt the disk
    // copy and never contact the token endpoint with the old token.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "stale refresh token must not be submitted"
        );
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    let stale = AuthFile {
        tokens: Tokens {
            id_token: "old-id".into(),
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            account_id: Some("account".into()),
        },
        last_refresh: Utc::now() - ChronoDuration::days(9),
    };
    let auth = CodexAuth::from_file(path.clone(), stale)
        .unwrap()
        .with_token_endpoint(endpoint);
    let fresh = AuthFile {
        tokens: Tokens {
            id_token: "new-id".into(),
            access_token: "new-access".into(),
            refresh_token: "new-refresh".into(),
            account_id: Some("account".into()),
        },
        last_refresh: Utc::now(),
    };
    save_file(&path, &fresh).unwrap();
    auth.refresh_if_needed(false, None).await.unwrap();
    server.await.unwrap();
    assert_eq!(
        auth.current_access_token_and_account().await.unwrap().0,
        "new-access"
    );
}

#[tokio::test]
async fn refresh_submits_disk_token_not_stale_memory_token() {
    // Memory holds R1 but another process rotated the disk to R2. The
    // exchange must submit R2 (which still needs refreshing), never R1.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..count]);
        assert!(request.contains("disk-refresh"));
        assert!(!request.contains("old-refresh"));
        let body = r#"{"access_token":"next-access","refresh_token":"next-refresh"}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    let memory = AuthFile {
        tokens: Tokens {
            id_token: "id".into(),
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            account_id: Some("account".into()),
        },
        last_refresh: Utc::now() - ChronoDuration::days(9),
    };
    let auth = CodexAuth::from_file(path.clone(), memory)
        .unwrap()
        .with_token_endpoint(endpoint);
    // Disk copy rotated by another process: same account, tokens that still
    // need refreshing (stale last_refresh => unknown-expiry path).
    let disk = AuthFile {
        tokens: Tokens {
            id_token: "id".into(),
            access_token: "disk-access".into(),
            refresh_token: "disk-refresh".into(),
            account_id: Some("account".into()),
        },
        last_refresh: Utc::now() - ChronoDuration::days(9),
    };
    save_file(&path, &disk).unwrap();
    auth.refresh_if_needed(false, None).await.unwrap();
    server.await.unwrap();
    assert_eq!(
        auth.current_access_token_and_account().await.unwrap().0,
        "next-access"
    );
}

#[tokio::test]
async fn reused_token_recovers_from_disk_rotation() {
    // Cross-process rotation race: we submit R1, the provider reports
    // refresh_token_reused because another process rotated R1 -> R2 on disk
    // while our request was in flight. The failure path must re-read the
    // file, adopt R2 and retry successfully instead of demanding a new login.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    let disk_path = path.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..count]).contains("old-refresh"));
        // The other process finishes its rotation right after we submitted
        // the stale token: write R2 to disk, then reject R1 as reused.
        let rotated = AuthFile {
            tokens: Tokens {
                id_token: "id".into(),
                access_token: "rotated-access".into(),
                refresh_token: "rotated-refresh".into(),
                account_id: Some("account".into()),
            },
            last_refresh: Utc::now() - ChronoDuration::days(9),
        };
        save_file(&disk_path, &rotated).unwrap();
        let body = r#"{"error":"refresh_token_reused","refresh_token":"do-not-leak"}"#;
        stream.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        // Retry with the adopted R2.
        let (mut stream, _) = listener.accept().await.unwrap();
        let count = stream.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..count]).contains("rotated-refresh"));
        let ok = r#"{"access_token":"final-access","refresh_token":"final-refresh"}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", ok.len(), ok).as_bytes()).await.unwrap();
    });
    let memory = AuthFile {
        tokens: Tokens {
            id_token: "id".into(),
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            account_id: Some("account".into()),
        },
        last_refresh: Utc::now() - ChronoDuration::days(9),
    };
    save_file(&path, &memory).unwrap();
    let auth = CodexAuth::from_file(path.clone(), memory)
        .unwrap()
        .with_token_endpoint(endpoint);
    auth.refresh_if_needed(false, None).await.unwrap();
    server.await.unwrap();
    assert_eq!(
        auth.current_access_token_and_account().await.unwrap().0,
        "final-access"
    );
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("final-refresh")
    );
}

#[tokio::test]
async fn disk_auth_with_different_account_is_not_adopted() {
    // The disk copy belongs to a different ChatGPT account: it must never be
    // adopted silently — the exchange proceeds with the in-memory token.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..count]).contains("memory-refresh"));
        let body = r#"{"access_token":"next-access","refresh_token":"next-refresh"}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    let memory = AuthFile {
        tokens: Tokens {
            id_token: "id-a".into(),
            access_token: "old-access".into(),
            refresh_token: "memory-refresh".into(),
            account_id: Some("account-a".into()),
        },
        last_refresh: Utc::now() - ChronoDuration::days(9),
    };
    let auth = CodexAuth::from_file(path.clone(), memory)
        .unwrap()
        .with_token_endpoint(endpoint);
    let other_account = AuthFile {
        tokens: Tokens {
            id_token: "id-b".into(),
            access_token: "other-access".into(),
            refresh_token: "other-refresh".into(),
            account_id: Some("account-b".into()),
        },
        last_refresh: Utc::now() - ChronoDuration::days(9),
    };
    save_file(&path, &other_account).unwrap();
    auth.refresh_if_needed(false, None).await.unwrap();
    server.await.unwrap();
    assert_eq!(
        auth.current_access_token_and_account().await.unwrap().0,
        "next-access"
    );
}

#[tokio::test]
async fn missing_or_corrupt_disk_auth_keeps_memory_logic() {
    // A missing or undecodable auth file must not panic or stall the refresh:
    // the exchange proceeds with the in-memory token as before.
    for (label, disk_content) in [("missing", None), ("corrupt", Some("not-json"))] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).contains("memory-refresh"));
            let body = r#"{"access_token":"next-access","refresh_token":"next-refresh"}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth.json");
        let memory = AuthFile {
            tokens: Tokens {
                id_token: "id".into(),
                access_token: "old-access".into(),
                refresh_token: "memory-refresh".into(),
                account_id: Some("account".into()),
            },
            last_refresh: Utc::now() - ChronoDuration::days(9),
        };
        if let Some(content) = disk_content {
            std::fs::write(&path, content).unwrap();
        }
        let auth = CodexAuth::from_file(path, memory)
            .unwrap()
            .with_token_endpoint(endpoint);
        auth.refresh_if_needed(false, None).await.unwrap();
        server.await.unwrap();
        assert_eq!(
            auth.current_access_token_and_account().await.unwrap().0,
            "next-access",
            "case: {label}"
        );
    }
}
