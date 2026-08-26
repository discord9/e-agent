use std::sync::{Arc, Barrier};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

fn web_search(endpoint: String) -> WebSearch {
    WebSearch::for_test("test-api-key".into(), endpoint, Duration::from_secs(1))
}

fn http_response(status: &str, body: impl AsRef<[u8]>) -> Vec<u8> {
    let body = body.as_ref();
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn empty_captured() -> Captured {
    Captured {
        bytes: Vec::new(),
        tail: Vec::new(),
        total: 0,
        truncated: false,
        full: Vec::new(),
    }
}

fn redirect_response(location: &str) -> Vec<u8> {
    format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    let header_end = loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0, "client closed before completing its request");
        request.extend_from_slice(&buffer[..count]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&request[..header_end]).unwrap();
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    while request.len() < header_end + content_length {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0, "client closed before sending its request body");
        request.extend_from_slice(&buffer[..count]);
    }
    request
}

async fn web_server(
    response: Vec<u8>,
    delay: Duration,
) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/context", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut socket).await;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let _ = socket.write_all(&response).await;
        request
    });
    (endpoint, task)
}

#[test]
fn web_search_spec_exposes_only_query() {
    assert_eq!(
            WebSearch::new("key".into()).spec(),
            ToolSpec {
                name: "web_search".into(),
                description: "Search public web documentation and code examples. Never include secrets, private source code, internal URLs, or personal data in the query.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "A specific public-web research query."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            }
        );
}

/// The always-on `read_output` tool is registered on EVERY session: main,
/// read-only main (with and without a sandbox), and subagent builds (the
/// `builtins_with_background` path).
#[test]
fn read_output_registered_for_main_read_only_and_subagent_builds() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let names = |tools: Vec<Box<dyn Tool>>| -> Vec<String> {
        let mut names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
        names.sort();
        names
    };

    // Main build (no key, no sandbox).
    let (tools, _) = builtins_with_exa_key(workspace.clone(), None, None, false, None, None);
    assert!(names(tools).contains(&"read_output".to_string()));

    // Read-only main build without a sandbox (fail-closed bash).
    let (tools, _) = builtins_with_exa_key(workspace.clone(), None, None, true, None, None);
    let n = names(tools);
    assert!(n.contains(&"read_output".to_string()), "{n:?}");

    // Subagent build (shared background registry).
    let background = BackgroundTasks::new(None, None);
    let sub = builtins_with_background(
        workspace.clone(),
        background.clone(),
        None,
        false,
        true,
        Some("sub-1".into()),
    );
    assert!(names(sub).contains(&"read_output".to_string()));

    // Read-only subagent build.
    let sub_ro = builtins_with_background(
        workspace,
        background,
        None,
        true,
        true,
        Some("sub-2".into()),
    );
    assert!(names(sub_ro).contains(&"read_output".to_string()));

    // The schema is CLOSED and the tool is read-only by construction: its
    // spec declares exactly ref/offset/limit with additionalProperties
    // false and a required ref.
    let (tools, _) = builtins_with_exa_key(
        Workspace::new(temp.path()).unwrap(),
        None,
        None,
        false,
        None,
        None,
    );
    let spec = tools
        .iter()
        .find(|tool| tool.spec().name == "read_output")
        .unwrap()
        .spec();
    assert_eq!(spec.parameters["additionalProperties"], false);
    assert_eq!(spec.parameters["required"], json!(["ref"]));
    let properties = spec.parameters["properties"].as_object().unwrap();
    let mut keys: Vec<&String> = properties.keys().collect();
    keys.sort();
    assert_eq!(keys, vec!["limit", "offset", "ref"]);
}

#[test]
fn web_search_registration_requires_a_nonempty_key() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let local_tools = vec![
        "read_file".to_string(),
        "read_image".to_string(),
        "write_file".to_string(),
        "edit_file".to_string(),
        "get_background_tasks".to_string(),
        "cancel_background_task".to_string(),
        "bash".to_string(),
        "get_goal".to_string(),
        "update_goal".to_string(),
        "read_output".to_string(),
    ];
    for key in [None, Some("   ".into())] {
        let (tools, _) = builtins_with_exa_key(workspace.clone(), key, None, false, None, None);
        let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
        assert_eq!(names, local_tools);
    }
    let (tools, _) =
        builtins_with_exa_key(workspace, Some(" key ".into()), None, false, None, None);
    let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
    assert_eq!(
        names,
        [
            "read_file",
            "read_image",
            "write_file",
            "edit_file",
            "get_background_tasks",
            "cancel_background_task",
            "bash",
            "web_search",
            "get_goal",
            "update_goal",
            "read_output"
        ]
        .map(String::from)
    );
}

#[tokio::test]
async fn web_search_sends_the_expected_request_and_returns_response() {
    let (endpoint, server) = web_server(
        http_response(
            "200 OK",
            br#"{"response":"public test-api-key context","requestId":"id"}"#,
        ),
        Duration::ZERO,
    )
    .await;
    let result = web_search(endpoint)
        .execute(json!({"query": "  Rust ownership docs  "}))
        .await
        .unwrap();
    assert_eq!(result.content, "public [redacted] context");

    let request = server.await.unwrap();
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    let headers = std::str::from_utf8(&request[..header_end]).unwrap();
    assert!(headers.starts_with("POST /context HTTP/1.1\r\n"));
    assert!(headers.contains("x-api-key: test-api-key\r\n"));
    assert!(headers.contains("content-type: application/json\r\n"));
    let body: Value = serde_json::from_slice(&request[header_end..]).unwrap();
    assert_eq!(
        body,
        json!({"query": "Rust ownership docs", "tokensNum": 5000})
    );
}

#[tokio::test]
async fn web_search_validates_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/context", listener.local_addr().unwrap());
    let tool = web_search(endpoint.clone());
    for arguments in [
        json!({"query": ""}),
        json!({"query": "   "}),
        json!({"query": 7}),
        json!({"query": "界".repeat(WEB_SEARCH_QUERY_LIMIT + 1)}),
    ] {
        assert!(tool.execute(arguments).await.is_err());
    }
    let invalid_key_tool =
        WebSearch::for_test("invalid\nkey".into(), endpoint, Duration::from_secs(1));
    let error = invalid_key_tool
        .execute(json!({"query": "public query"}))
        .await
        .unwrap_err();
    assert_eq!(error, "web search API key is invalid");
    assert!(!error.contains("invalid\nkey"));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn web_search_provider_errors_are_bounded_and_redact_the_key() {
    for status in [
        "401 Unauthorized",
        "402 Payment Required",
        "429 Too Many Requests",
        "500 Internal Server Error",
    ] {
        let (endpoint, server) = web_server(
            http_response(
                status,
                format!("provider error for test-api-key ({status})"),
            ),
            Duration::ZERO,
        )
        .await;
        let error = web_search(endpoint)
            .execute(json!({"query": "public query"}))
            .await
            .unwrap_err();
        assert!(error.contains(status.split_whitespace().next().unwrap()));
        assert!(error.contains("provider error"));
        assert!(!error.contains("test-api-key"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn web_search_does_not_follow_redirects() {
    let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let location = format!("http://{}/context", second.local_addr().unwrap());
    let (endpoint, first) = web_server(redirect_response(&location), Duration::ZERO).await;

    let error = web_search(endpoint)
        .execute(json!({"query": "public query"}))
        .await
        .unwrap_err();
    assert!(error.contains("302"));
    first.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), second.accept())
            .await
            .is_err(),
        "redirect target received a request"
    );
}

#[tokio::test]
async fn web_search_handles_malformed_missing_and_unavailable_responses() {
    for body in [
        br#"{"response":"unterminated"#.as_slice(),
        br#"{}"#.as_slice(),
    ] {
        let (endpoint, server) = web_server(http_response("200 OK", body), Duration::ZERO).await;
        assert!(
            web_search(endpoint)
                .execute(json!({"query": "public query"}))
                .await
                .unwrap_err()
                .contains("malformed JSON or no response")
        );
        server.await.unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/context", listener.local_addr().unwrap());
    drop(listener);
    assert_eq!(
        web_search(endpoint)
            .execute(json!({"query": "public query"}))
            .await
            .unwrap_err(),
        "web search request failed"
    );

    let (endpoint, server) = web_server(
        http_response("200 OK", br#"{"response":"late"}"#),
        Duration::from_millis(250),
    )
    .await;
    let timeout_tool =
        WebSearch::for_test("test-api-key".into(), endpoint, Duration::from_millis(25));
    assert_eq!(
        timeout_tool
            .execute(json!({"query": "public query"}))
            .await
            .unwrap_err(),
        "web search request failed"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn web_search_caps_success_and_error_bodies() {
    let response = json!({"response": "界".repeat(OUTPUT_LIMIT / 2)}).to_string();
    let (endpoint, server) = web_server(http_response("200 OK", response), Duration::ZERO).await;
    let result = web_search(endpoint)
        .execute(json!({"query": "public query"}))
        .await
        .unwrap();
    assert!(result.content.len() <= OUTPUT_LIMIT);
    assert!(result.content.is_char_boundary(result.content.len()));
    assert!(result.content.ends_with("\n...[truncated]"));
    server.await.unwrap();

    let response = json!({"response": "x".repeat(WEB_SEARCH_RESPONSE_LIMIT * 2)}).to_string();
    let (endpoint, server) = web_server(http_response("200 OK", response), Duration::ZERO).await;
    assert!(
        web_search(endpoint)
            .execute(json!({"query": "public query"}))
            .await
            .unwrap_err()
            .contains("response body exceeds")
    );
    server.await.unwrap();

    let (endpoint, server) = web_server(
        http_response(
            "500 Internal Server Error",
            "e".repeat(WEB_SEARCH_ERROR_PREVIEW_LIMIT * 2),
        ),
        Duration::ZERO,
    )
    .await;
    let error = web_search(endpoint)
        .execute(json!({"query": "public query"}))
        .await
        .unwrap_err();
    assert!(
        error.len()
            <= "web search failed with status 500 Internal Server Error: ".len()
                + WEB_SEARCH_ERROR_PREVIEW_LIMIT
    );
    assert!(error.ends_with("\n...[truncated]"));
    server.await.unwrap();
}

async fn edit(temp: &tempfile::TempDir, old: &str, new: &str) -> Result<ToolOutput, String> {
    EditFile {
        workspace: Workspace::new(temp.path()).unwrap(),
    }
    .execute(json!({"path": "file.txt", "old": old, "new": new}))
    .await
}

#[tokio::test]
async fn edit_requires_exactly_one_match() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.txt");
    std::fs::write(&path, "one two one").unwrap();
    // 0 matches: guide the model to re-read (via read_file) and copy the
    // exact text verbatim.
    let zero = edit(&temp, "missing", "x").await.unwrap_err();
    assert!(zero.contains("found 0"), "{zero}");
    assert!(zero.contains("use `read_file`"), "{zero}");
    assert!(
        zero.contains("re-read the file and copy the exact text verbatim"),
        "{zero}"
    );
    // Multiple matches: guide the model to add context until unique.
    let many = edit(&temp, "one", "x").await.unwrap_err();
    assert!(many.contains("found 2"), "{many}");
    assert!(
        many.contains("add more surrounding context to `old` until it matches exactly once"),
        "{many}"
    );
    assert_eq!(
        edit(&temp, "two", "x").await.unwrap().content,
        "file edited (line 1)"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "one x one");
}

#[tokio::test]
async fn edit_preserves_crlf_line_endings() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.txt");
    // CRLF file (Windows-style); the model's old/new use LF.
    std::fs::write(&path, "line one\r\nline two\r\nline three\r\n").unwrap();
    let result = edit(&temp, "line two", "line TWO").await.unwrap().content;
    assert_eq!(result, "file edited (line 2)");
    // The whole file must still be CRLF — no fake whole-line diffs.
    let after = std::fs::read_to_string(path).unwrap();
    assert_eq!(after, "line one\r\nline TWO\r\nline three\r\n");
    // Every line ending is CRLF (no lone LF introduced by the edit).
    assert_eq!(after.matches("\r\n").count(), 3);
    assert_eq!(after.replace("\r\n", "").matches('\n').count(), 0);
}

#[tokio::test]
async fn edit_matches_lf_old_against_crlf_file() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.txt");
    // Old text passed by the model without \r must still match a CRLF file.
    std::fs::write(&path, "alpha\r\nbeta\r\n").unwrap();
    let result = edit(&temp, "beta", "BETA").await.unwrap().content;
    assert_eq!(result, "file edited (line 2)");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "alpha\r\nBETA\r\n");
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Undo stack
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Serialize undo tests against the process-global undo stack and reset it,
/// so each scenario starts from a known empty state.
async fn undo_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = UNDO_TEST_LOCK.lock().await;
    clear_undo_stack();
    guard
}

#[tokio::test]
async fn undo_write_restores_old_content() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.txt");
    std::fs::write(&path, "old content").unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    WriteFile {
        workspace: workspace.clone(),
    }
    .execute(json!({"path": "file.txt", "content": "new content"}))
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");

    let message = undo_file_op().unwrap();
    assert!(message.contains("已撤销 write_file: file.txt"), "{message}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old content");
    // An undone operation is gone from the stack: nothing left to undo.
    assert!(undo_file_op().unwrap_err().contains("没有可撤销"));
}

#[tokio::test]
async fn undo_edit_reverses_fragments() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.txt");
    std::fs::write(&path, "one two three").unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    EditFile {
        workspace: workspace.clone(),
    }
    .execute(json!({"path": "file.txt", "old": "two", "new": "TWO"}))
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one TWO three");

    let message = undo_file_op().unwrap();
    assert!(message.contains("已撤销 edit_file: file.txt"), "{message}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two three");
}

#[tokio::test]
async fn undo_created_write_deletes_the_file() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    WriteFile {
        workspace: workspace.clone(),
    }
    .execute(json!({"path": "new.txt", "content": "brand new"}))
    .await
    .unwrap();
    assert!(temp.path().join("new.txt").exists());

    let message = undo_file_op().unwrap();
    assert!(message.contains("已撤销 write_file: new.txt"), "{message}");
    assert!(!temp.path().join("new.txt").exists());
}

#[tokio::test]
async fn undo_fails_when_file_modified_afterwards() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.txt");
    let workspace = Workspace::new(temp.path()).unwrap();

    // edit path: the `new` fragment has been overwritten by a later edit.
    std::fs::write(&path, "one two three").unwrap();
    EditFile {
        workspace: workspace.clone(),
    }
    .execute(json!({"path": "file.txt", "old": "two", "new": "TWO"}))
    .await
    .unwrap();
    std::fs::write(&path, "one THREE three").unwrap();
    let error = undo_file_op().unwrap_err();
    assert!(error.contains("无法撤销"), "{error}");
    assert!(error.contains("已被后续修改"), "{error}");

    // write path: the written content has been tampered with.
    clear_undo_stack();
    std::fs::write(&path, "original").unwrap();
    WriteFile {
        workspace: workspace.clone(),
    }
    .execute(json!({"path": "file.txt", "content": "written"}))
    .await
    .unwrap();
    std::fs::write(&path, "tampered").unwrap();
    let error = undo_file_op().unwrap_err();
    assert!(error.contains("无法撤销"), "{error}");
    assert!(error.contains("已被后续修改"), "{error}");
}

#[tokio::test]
async fn undo_stack_is_bounded() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    for i in 0..UNDO_STACK_LIMIT + 5 {
        WriteFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": format!("f{i}.txt"), "content": format!("content {i}")}))
        .await
        .unwrap();
    }
    let stack = UNDO_STACK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(stack.len(), UNDO_STACK_LIMIT);
    // The oldest operations were dropped, the newest kept.
    assert_eq!(stack.first().unwrap().path, "f5.txt");
    assert_eq!(
        stack.last().unwrap().path,
        format!("f{}.txt", UNDO_STACK_LIMIT + 4)
    );
}

async fn read(temp: &tempfile::TempDir, arguments: Value) -> Result<ToolOutput, String> {
    ReadFile {
        workspace: Workspace::new(temp.path()).unwrap(),
    }
    .execute(arguments)
    .await
}

#[tokio::test]
async fn external_absolute_file_tools_enforce_policy_and_reuse_semantics() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let readable = temp.path().join("readable");
    let writable = temp.path().join("writable");
    std::fs::create_dir(&workspace_dir).unwrap();
    std::fs::create_dir(&readable).unwrap();
    std::fs::create_dir(&writable).unwrap();
    std::fs::write(readable.join("file"), "r1\nr2\nr3\n").unwrap();
    std::fs::write(writable.join("file"), "one two one").unwrap();
    let policy = crate::config::Sandbox {
        enabled: false,
        workspace_writable: false,
        readable_paths: vec![readable.to_str().unwrap().to_owned()],
        writable_paths: vec![writable.to_str().unwrap().to_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    let read_tool = ReadFile {
        workspace: workspace.clone(),
    };
    assert!(
        read_tool
            .execute(json!({"path": readable.join("file"), "limit": 2}))
            .await
            .unwrap()
            .content
            .contains("use offset 3")
    );
    let write_tool = WriteFile {
        workspace: workspace.clone(),
    };
    assert!(
        write_tool
            .execute(json!({"path": readable.join("file"), "content": "no"}))
            .await
            .is_err()
    );
    write_tool
        .execute(json!({"path": writable.join("new"), "content": "created"}))
        .await
        .unwrap();
    write_tool
        .execute(json!({"path": "relative", "content": "allowed"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace_dir.join("relative")).unwrap(),
        "allowed"
    );
    let edit_tool = EditFile { workspace };
    assert!(
        edit_tool
            .execute(json!({"path": readable.join("file"), "old": "r1", "new": "x"}))
            .await
            .is_err()
    );
    assert!(
        edit_tool
            .execute(json!({"path": writable.join("file"), "old": "one", "new": "x"}))
            .await
            .unwrap_err()
            .contains("found 2")
    );
    edit_tool
        .execute(json!({"path": writable.join("file"), "old": "two", "new": "x"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(writable.join("file")).unwrap(),
        "one x one"
    );
}

#[tokio::test]
async fn policy_file_tools_allow_read_but_reject_relative_and_external_absolute_writes() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".e-agent")).unwrap();
    let policy_path = temp.path().join(".e-agent/config.toml");
    std::fs::write(&policy_path, "[sandbox]\n").unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![temp.path().to_str().unwrap().to_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(temp.path())
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    let read = ReadFile {
        workspace: workspace.clone(),
    };
    assert!(
        read.execute(json!({"path": ".e-agent/config.toml"}))
            .await
            .unwrap()
            .content
            .contains("[sandbox]")
    );
    assert!(
        read.execute(json!({"path": policy_path}))
            .await
            .unwrap()
            .content
            .contains("[sandbox]")
    );
    for path in [
        ".e-agent/config.toml".to_owned(),
        policy_path.to_str().unwrap().to_owned(),
    ] {
        let error = WriteFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": path, "content": ""}))
        .await
        .unwrap_err();
        assert!(error.contains("outside the agent"));
    }
    let error = EditFile { workspace }
        .execute(json!({
            "path": policy_path,
            "old": "sandbox",
            "new": "x"
        }))
        .await
        .unwrap_err();
    assert!(error.contains("outside the agent"));
}

#[cfg(unix)]
#[tokio::test]
async fn policy_file_tools_reject_workspace_symlink_alias_writes() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".e-agent")).unwrap();
    std::fs::write(temp.path().join(".e-agent/config.toml"), "[sandbox]\n").unwrap();
    symlink(".e-agent/config.toml", temp.path().join("policy-alias")).unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    assert!(
        ReadFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": "policy-alias"}))
        .await
        .unwrap()
        .content
        .contains("[sandbox]")
    );
    let error = WriteFile { workspace }
        .execute(json!({"path": "policy-alias", "content": "no"}))
        .await
        .unwrap_err();
    assert!(error.contains("outside the agent"));
}

#[tokio::test]
async fn read_pages_lines_with_a_continuation_hint() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file.txt"), "a\nb\nc\nd\ne\n").unwrap();
    assert_eq!(
        read(&temp, json!({"path": "file.txt", "limit": 2}))
            .await
            .unwrap()
            .content,
        "a\nb\n[showing lines 1-2 of 5; use offset 3 to continue]"
    );
    assert_eq!(
        read(&temp, json!({"path": "file.txt", "offset": 3, "limit": 2}))
            .await
            .unwrap()
            .content,
        "c\nd\n[showing lines 3-4 of 5; use offset 5 to continue]"
    );
    assert_eq!(
        read(&temp, json!({"path": "file.txt", "offset": 5}))
            .await
            .unwrap()
            .content,
        "e"
    );
}

#[tokio::test]
async fn read_reports_offset_past_end_and_empty_files() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file.txt"), "a\nb\n").unwrap();
    assert_eq!(
        read(&temp, json!({"path": "file.txt", "offset": 9}))
            .await
            .unwrap()
            .content,
        "[offset 9 is past end of file (2 lines)]"
    );
    std::fs::write(temp.path().join("empty.txt"), "").unwrap();
    assert_eq!(
        read(&temp, json!({"path": "empty.txt"}))
            .await
            .unwrap()
            .content,
        "[empty file]"
    );
}

#[tokio::test]
async fn read_truncates_long_lines_to_the_byte_limit() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file.txt"), "x".repeat(READ_LIMIT + 1)).unwrap();
    let output = read(&temp, json!({"path": "file.txt"}))
        .await
        .unwrap()
        .content;
    assert!(output.ends_with("\n...[truncated]"));
    assert_eq!(output.len(), READ_LIMIT + "\n...[truncated]".len());
}

#[tokio::test]
async fn read_truncates_multibyte_lines_at_a_char_boundary() {
    let temp = tempfile::tempdir().unwrap();
    // "测" is 3 bytes in UTF-8 and READ_LIMIT % 3 == 1, so byte READ_LIMIT
    // always lands inside a character. The old `output.truncate(READ_LIMIT)`
    // panicked here (`assertion failed: self.is_char_boundary`); the fixed
    // version must back off to the nearest char boundary.
    let content = "测".repeat(READ_LIMIT / 3 + 10);
    std::fs::write(temp.path().join("cjk.txt"), &content).unwrap();
    let output = read(&temp, json!({"path": "cjk.txt"}))
        .await
        .unwrap()
        .content;
    assert!(output.ends_with("\n...[truncated]"));
    // Valid UTF-8 throughout: the truncation point is a char boundary.
    assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    // Truncated at READ_LIMIT rounded down to a char boundary (65535), plus
    // the marker on top (same budget semantics as the ASCII case above).
    assert_eq!(
        output.len(),
        (READ_LIMIT / 3) * 3 + "\n...[truncated]".len()
    );
    assert!(output.starts_with(&"测".repeat(READ_LIMIT / 3)));
}

#[tokio::test]
async fn read_rejects_invalid_paging_arguments() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file.txt"), "a\n").unwrap();
    assert!(
        read(&temp, json!({"path": "file.txt", "offset": 0}))
            .await
            .unwrap_err()
            .contains(">= 1")
    );
    assert!(
        read(&temp, json!({"path": "file.txt", "limit": 0}))
            .await
            .unwrap_err()
            .contains(">= 1")
    );
    assert!(
        read(&temp, json!({"path": "file.txt", "offset": "x"}))
            .await
            .unwrap_err()
            .contains("non-negative integer")
    );
}

#[tokio::test]
async fn write_creates_a_new_nested_file() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let tool = WriteFile {
        workspace: Workspace::new(temp.path()).unwrap(),
    };
    assert_eq!(
        tool.execute(json!({"path": "new/file.txt", "content": "hello"}))
            .await
            .unwrap()
            .content,
        "file written"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("new/file.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn capture_splits_truncated_streams_into_head_and_tail() {
    async fn captured(bytes: Vec<u8>) -> Captured {
        let (mut writer, reader) = tokio::io::duplex(1024);
        tokio::spawn(async move { writer.write_all(&bytes).await.unwrap() });
        capture(reader, None, None).await.unwrap()
    }

    // Well under the budget: full text, no truncation marker.
    let short = captured(vec![b'a'; 1024]).await;
    let output = format_output(Some(0), &short, &empty_captured());
    assert!(!short.truncated);
    assert!(output.contains("aaa"), "{output}");
    assert!(!output.contains("[truncated"), "{output}");

    // A stream straddling HEAD_LIMIT but still within the budget is
    // reconstructed exactly (head + overlapping tail window glued back).
    let mid = captured(vec![b'b'; HEAD_LIMIT + TAIL_LIMIT / 2]).await;
    let output = format_output(Some(0), &mid, &empty_captured());
    assert!(!mid.truncated);
    assert!(!output.contains("[truncated"), "{output}");
    assert_eq!(output.matches('b').count(), HEAD_LIMIT + TAIL_LIMIT / 2);

    // Over the budget: head start survives, tail end survives, middle marked.
    let big = captured(vec![b'o'; 100 * 1024]).await;
    assert!(big.truncated);
    assert_eq!(big.bytes.len(), HEAD_LIMIT);
    assert_eq!(big.tail.len(), TAIL_LIMIT);
    let output = format_output(Some(0), &big, &empty_captured());
    assert!(
        output.starts_with("exit code: 0\nstdout:\noooo"),
        "{output}"
    );
    assert!(
        output.contains(&format!(
            "[truncated: {} bytes omitted]",
            100 * 1024 - HEAD_LIMIT - TAIL_LIMIT
        )),
        "{output}"
    );
    let stdout_section = output.split("\nstderr:\n").next().unwrap();
    assert!(stdout_section.trim_end().ends_with('o'), "{output}");
    assert!(output.contains("\nstderr:\n"), "{output}");

    // Both streams truncated: each gets its own head + marker + tail.
    let err = captured(vec![b'e'; 80 * 1024]).await;
    let output = format_output(Some(1), &big, &err);
    assert!(output.contains(&format!(
        "[truncated: {} bytes omitted]",
        100 * 1024 - HEAD_LIMIT - TAIL_LIMIT
    )));
    assert!(output.contains(&format!(
        "[truncated: {} bytes omitted]",
        80 * 1024 - HEAD_LIMIT - TAIL_LIMIT
    )));
}

#[tokio::test]
async fn capture_truncation_keeps_utf8_boundaries() {
    async fn captured(bytes: Vec<u8>) -> Captured {
        let (mut writer, reader) = tokio::io::duplex(1024);
        tokio::spawn(async move { writer.write_all(&bytes).await.unwrap() });
        capture(reader, None, None).await.unwrap()
    }
    // "a" + 你×21846 = 65539 bytes > OUTPUT_LIMIT: both the head cut (at
    // 49152) and the tail window front land inside a 3-byte character.
    let mut bytes = vec![b'a'];
    for _ in 0..21846 {
        bytes.extend_from_slice("你".as_bytes());
    }
    assert!(bytes.len() > OUTPUT_LIMIT);
    let captured = captured(bytes).await;
    assert!(captured.truncated);
    let rendered = format_output(Some(1), &captured, &empty_captured());
    // No split multibyte char: from_utf8_lossy must not emit U+FFFD.
    assert!(!rendered.contains('\u{FFFD}'), "{rendered}");
    assert!(rendered.contains("[truncated: "), "{rendered}");
    let stdout_section = rendered.split("stderr:\n").next().unwrap();
    // Head ends and tail starts on complete 你 characters (the head section
    // carries the separator newline before the marker, so trim it).
    let head = stdout_section.split("[truncated: ").next().unwrap();
    assert!(head.trim_end().ends_with('你'), "{rendered}");
    let after_marker = stdout_section.split("[truncated: ").nth(1).unwrap();
    let tail = after_marker.split_once('\n').unwrap().1;
    assert!(tail.starts_with('你'), "{rendered}");
}

#[cfg(unix)]
#[tokio::test]
async fn failed_truncated_bash_writes_full_output_log() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let shell = Shell::detect().unwrap();
    let text = run_bash(
        &shell,
        &workspace,
        "printf 'x%.0s' {1..100000}; exit 1",
        None,
        false,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    // The displayed text is head + tail with a marker and a log hint.
    assert!(text.contains("[truncated: "), "{text}");
    let path = text
        .split("[full output: ")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .expect("failed+truncated output must hint the full log");
    let expected_dir = temp
        .path()
        .join(".e-agent/logs/")
        .to_string_lossy()
        .into_owned();
    assert!(path.starts_with(&expected_dir), "hint path {path}");
    // The log holds the complete untruncated stdout (100000 'x') plus the
    // command echo and stream separators.
    let content = std::fs::read_to_string(path).unwrap();
    assert!(content.starts_with("$ printf"), "{content}");
    assert!(content.contains("--- stdout ---"), "{content}");
    assert!(content.contains("--- stderr ---"), "{content}");
    let stdout_section = content.split("--- stdout ---").nth(1).unwrap();
    let stdout_only = stdout_section.split("--- stderr ---").next().unwrap();
    assert_eq!(stdout_only.matches('x').count(), 100000);
}

#[cfg(unix)]
#[tokio::test]
async fn successful_long_bash_output_does_not_write_a_log() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let shell = Shell::detect().unwrap();
    let text = run_bash(
        &shell,
        &workspace,
        "printf 'y%.0s' {1..100000}",
        None,
        false,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    // Truncated display, but success → no persistence, no hint.
    assert!(text.contains("[truncated: "), "{text}");
    assert!(!text.contains("[full output: "), "{text}");
    assert!(!temp.path().join(".e-agent/logs").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn unsandboxed_background_shell_dies_with_parent() {
    // The child test process is the helper parent. The shell uses `exec` so
    // the marker is the directly spawned top-level process, not a descendant
    // that would require process-tree containment.
    if let Some(marker) = std::env::var_os("E_AGENT_PDEATH_MARKER") {
        let marker = std::path::PathBuf::from(marker);
        let workspace = Workspace::new(marker.parent().expect("marker parent")).unwrap();
        let shell = Shell::detect().unwrap();
        let slot = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let token = std::env::var("E_AGENT_PDEATH_TOKEN").expect("missing pdeath token");
        let command = format!(
            "echo $$ > {}; exec -a {} /bin/sleep 30",
            marker.display(),
            token
        );
        let _ = run_bash(
            &shell,
            &workspace,
            &command,
            None,
            false,
            Some(slot),
            None,
            None,
            None,
            None,
        )
        .await;
        return;
    }

    use std::os::fd::AsFd;

    struct PdeathGuard {
        helper: Option<std::process::Child>,
        helper_fd: rustix::fd::OwnedFd,
        marker_fd: Option<rustix::fd::OwnedFd>,
    }

    impl PdeathGuard {
        fn kill_helper_and_wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            let _ =
                rustix::process::pidfd_send_signal(&self.helper_fd, rustix::process::Signal::KILL);
            self.helper.take().expect("helper already reaped").wait()
        }

        fn kill_marker(&self) {
            if let Some(fd) = &self.marker_fd {
                let _ = rustix::process::pidfd_send_signal(fd, rustix::process::Signal::KILL);
            }
        }
    }

    impl Drop for PdeathGuard {
        fn drop(&mut self) {
            self.kill_marker();
            let _ =
                rustix::process::pidfd_send_signal(&self.helper_fd, rustix::process::Signal::KILL);
            if let Some(helper) = self.helper.as_mut() {
                let _ = helper.wait();
            }
        }
    }

    async fn marker_is_terminated(marker_fd: &rustix::fd::OwnedFd) {
        // pidfds become readable when their process exits. Unlike waitid,
        // polling a pidfd does not require the caller to be the process's
        // parent, so this remains valid after the marker is reparented.
        let async_fd = tokio::io::unix::AsyncFd::new(rustix::io::dup(marker_fd.as_fd()).unwrap())
            .expect("failed to register marker pidfd for polling");
        let mut readiness = async_fd
            .readable()
            .await
            .expect("failed to poll marker pidfd");
        readiness.clear_ready();
    }

    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("top-level.pid");
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let marker_token = format!("e-agent-pdeath-marker-{unique}");
    let exe = std::env::current_exe().expect("cannot locate the test binary");
    let helper = std::process::Command::new(exe)
        .args([
            "--exact",
            "tools::tests::unsandboxed_background_shell_dies_with_parent",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("E_AGENT_PDEATH_MARKER", &marker)
        .env("E_AGENT_PDEATH_TOKEN", &marker_token)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn the helper parent");
    let helper_pid = helper.id() as i32;
    let helper_fd = rustix::process::pidfd_open(
        rustix::process::Pid::from_raw(helper_pid).expect("invalid helper pid"),
        rustix::process::PidfdFlags::empty(),
    )
    .expect("pidfds are required for identity-safe cleanup");
    let mut guard = PdeathGuard {
        helper: Some(helper),
        helper_fd,
        marker_fd: None,
    };

    let marker_ready = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        marker_ready.is_ok(),
        "helper did not publish the top-level process pid"
    );
    let marker_pid: i32 = std::fs::read_to_string(&marker)
        .expect("top-level pid marker disappeared")
        .trim()
        .parse()
        .expect("top-level pid marker was not numeric");
    let marker_fd = rustix::process::pidfd_open(
        rustix::process::Pid::from_raw(marker_pid).expect("invalid marker pid"),
        rustix::process::PidfdFlags::empty(),
    )
    .expect("pidfds are required for identity-safe cleanup");
    guard.marker_fd = Some(marker_fd);

    // Pin identity before accepting the published PID. The pidfd then makes
    // all later cleanup identity-safe.
    let stat = std::fs::read_to_string(format!("/proc/{marker_pid}/stat"))
        .expect("marker process disappeared before identity check");
    let fields: Vec<&str> = stat
        .split_once(") ")
        .expect("invalid marker stat")
        .1
        .split_whitespace()
        .collect();
    let marker_ppid: i32 = fields
        .get(1)
        .expect("marker stat missing ppid")
        .parse()
        .expect("marker ppid was not numeric");
    assert_eq!(
        marker_ppid, helper_pid,
        "marker was not still owned by helper"
    );
    let exec_ready = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(cmdline) = std::fs::read(format!("/proc/{marker_pid}/cmdline"))
                && cmdline.split(|byte| *byte == 0).next() == Some(marker_token.as_bytes())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        exec_ready.is_ok(),
        "marker process did not reach its unique argv"
    );
    // Killing the helper exercises the parent-death path rather than the
    // normal process-group cancellation guard.
    let helper_status = guard
        .kill_helper_and_wait()
        .expect("failed to reap helper parent");
    assert!(
        !helper_status.success(),
        "helper was not killed: {helper_status}"
    );

    let gone = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(marker_fd) = &guard.marker_fd {
                marker_is_terminated(marker_fd).await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    // Cleanup is deliberately before the assertion: a failed check must not
    // leave the directly spawned process behind.
    if gone.is_err() {
        guard.kill_marker();
    }
    assert!(
        gone.is_ok(),
        "top-level bash process survived helper SIGKILL"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_timeout_kills_its_background_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("child.pid");
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_millis(100)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30 * 60)), None),
        sandbox: None,
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    assert!(
        tool.execute(json!({"command": "sleep 30 & echo $! > child.pid; wait"}))
            .await
            .unwrap_err()
            .contains("timed out")
    );
    let pid = std::fs::read_to_string(pid_file).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let status = std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.trim())
        .status()
        .unwrap();
    assert!(!status.success(), "background child survived timeout");
}

#[test]
fn bash_description_explains_the_sandbox_only_when_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let background = BackgroundTasks::new(Some(Duration::from_secs(30)), None);

    // No sandbox: plain description, no sandbox caveats.
    let plain = Bash {
        workspace: workspace.clone(),
        timeout: Some(Duration::from_secs(1)),
        sender: None,
        background: background.clone(),
        sandbox: None,
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    let plain_desc = plain.spec().description;
    #[cfg(windows)]
    assert!(
        !plain_desc.contains("restricted primary token"),
        "{plain_desc}"
    );
    #[cfg(not(windows))]
    assert!(!plain_desc.contains("sandbox"), "{plain_desc}");

    // Sandboxed with writable workspace AND protect_git = true (subagent).
    let sandboxed = Bash {
        workspace: workspace.clone(),
        timeout: Some(Duration::from_secs(1)),
        sender: None,
        background: background.clone(),
        sandbox: Some(crate::config::Sandbox {
            enabled: true,
            network: false,
            workspace_writable: true,
            writable_paths: vec!["/mnt/big/cargo-home".into()],
            readable_paths: vec!["~/.rustup".into()],
            readable_mounts: Vec::new(),
            writable_mounts: Vec::new(),
        }),
        protect_git: true,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    let desc = sandboxed.spec().description;
    #[cfg(windows)]
    {
        assert!(desc.contains("Windows restricted primary token"), "{desc}");
        assert!(!desc.to_lowercase().contains("bubblewrap"), "{desc}");
        assert!(
            desc.contains("workspace is an allowed write root"),
            "{desc}"
        );
        assert!(desc.contains("not read isolation"), "{desc}");
        assert!(desc.contains("Everyone"), "{desc}");
    }
    #[cfg(not(windows))]
    {
        assert!(desc.contains("bubblewrap sandbox"), "{desc}");
        assert!(desc.contains("workspace is writable"), "{desc}");
    }
    #[cfg(not(windows))]
    {
        assert!(desc.contains("OUTSIDE the sandbox"), "{desc}");
        assert!(desc.contains("network is disabled"), "{desc}");
        assert!(desc.contains("~/.rustup"), "{desc}");
        assert!(desc.contains("read_file/write_file/edit_file"), "{desc}");
        assert!(desc.contains("linked-worktree pointer"), "{desc}");
        assert!(desc.contains("read-only to prevent"), "{desc}");
    }
    assert!(desc.contains("/mnt/big/cargo-home"), "{desc}");
    #[cfg(windows)]
    assert!(desc.contains("protect_git = false"), "{desc}");
    #[cfg(not(windows))]
    assert!(desc.contains("`.git`"), "{desc}");

    // Sandboxed with read-only workspace, protect_git = false (main).
    let sandboxed_ro = Bash {
        workspace: workspace.clone(),
        timeout: Some(Duration::from_secs(1)),
        sender: None,
        background: background.clone(),
        sandbox: Some(crate::config::Sandbox {
            enabled: true,
            network: true,
            workspace_writable: false,
            writable_paths: vec![],
            readable_paths: vec![],
            readable_mounts: Vec::new(),
            writable_mounts: Vec::new(),
        }),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    let desc_ro = sandboxed_ro.spec().description;
    #[cfg(windows)]
    assert!(
        desc_ro.contains("workspace is not an allowed write root"),
        "{desc_ro}"
    );
    #[cfg(not(windows))]
    assert!(desc_ro.contains("workspace is read-only"), "{desc_ro}");
    // Use a precise marker: the protect-git text includes backtick-wrapped `.git`.
    assert!(
        !desc_ro.contains("`.git`"),
        "main agent description must not claim .git is read-only: {desc_ro}"
    );

    // Sandboxed with writable workspace, protect_git = false (main):
    // must NOT mention .git.
    let sandboxed_main = Bash {
        workspace,
        timeout: Some(Duration::from_secs(1)),
        sender: None,
        background,
        sandbox: Some(crate::config::Sandbox {
            enabled: true,
            network: true,
            workspace_writable: true,
            writable_paths: vec![],
            readable_paths: vec![],
            readable_mounts: Vec::new(),
            writable_mounts: Vec::new(),
        }),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    let desc_main = sandboxed_main.spec().description;
    assert!(
        !desc_main.contains("`.git`"),
        "main agent description must not claim .git is read-only: {desc_main}"
    );
}

#[test]
fn read_only_builtins_exclude_write_edit_and_bash_without_sandbox() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (tools, _) = builtins_with_exa_key(workspace, None, None, true, None, None);
    let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
    assert_eq!(
        names,
        [
            "read_file",
            "get_background_tasks",
            "cancel_background_task",
            "get_goal",
            "update_goal",
            "read_output"
        ],
        "read-only without a sandbox: no write/edit and fail-closed no bash"
    );
}

#[test]
fn read_only_builtins_keep_bash_with_a_narrowed_sandbox() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (tools, _) = builtins_with_exa_key(
        workspace,
        Some("key".into()),
        Some(crate::config::Sandbox {
            enabled: true,
            network: true,
            workspace_writable: true,
            writable_paths: vec!["/mnt/big/cargo-home".into()],
            readable_paths: vec!["~/.rustup".into()],
            readable_mounts: Vec::new(),
            writable_mounts: Vec::new(),
        }),
        true,
        None,
        None,
    );
    let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
    #[cfg(windows)]
    let shell_name = "pwsh";
    #[cfg(not(windows))]
    let shell_name = "bash";
    assert_eq!(
        names,
        [
            "read_file",
            "get_background_tasks",
            "cancel_background_task",
            shell_name,
            "web_search",
            "get_goal",
            "update_goal",
            "read_output"
        ],
        "read-only with a sandbox keeps the shell and web_search"
    );
    // The shell description must reflect the narrowed policy.
    let bash_desc = tools
        .iter()
        .find(|tool| tool.spec().name == shell_name)
        .unwrap()
        .spec()
        .description;
    #[cfg(windows)]
    {
        assert!(
            bash_desc.contains("workspace is not an allowed write root"),
            "{bash_desc}"
        );
        assert!(bash_desc.contains("no network isolation"), "{bash_desc}");
    }
    #[cfg(not(windows))]
    {
        assert!(bash_desc.contains("workspace is read-only"), "{bash_desc}");
        assert!(
            !bash_desc.contains("network is disabled"),
            "read-only bash follows the main network=true: {bash_desc}"
        );
        assert!(
            bash_desc.contains("~/.rustup"),
            "readable roots survive the narrowing: {bash_desc}"
        );
    }
    assert!(
        !bash_desc.contains("/mnt/big/cargo-home"),
        "writable roots are dropped from the description: {bash_desc}"
    );
}

#[test]
fn read_only_sandbox_derivation_narrows_and_keeps_readable_roots() {
    let sandbox = crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: vec!["/mnt/big/cargo-home".into()],
        readable_paths: vec!["~/.rustup".into(), "~/.local".into()],
        writable_mounts: vec![("/mnt/big/cargo-home".into(), "/mnt/big/cargo-home".into())],
        readable_mounts: vec![("/home/x/.rustup".into(), "/home/x/.rustup".into())],
    };
    let narrowed = read_only_sandbox(&sandbox);
    assert!(narrowed.enabled);
    assert!(
        narrowed.network,
        "network follows the main config (true here) — read-only roles keep read-only network ops"
    );
    assert!(!narrowed.workspace_writable, "workspace must be read-only");
    assert!(
        narrowed.writable_paths.is_empty(),
        "extra writable roots must be dropped"
    );
    assert_eq!(
        narrowed.readable_paths,
        vec!["~/.rustup".to_owned(), "~/.local".to_owned()],
        "readable roots must be preserved"
    );
    assert!(
        narrowed.writable_mounts.is_empty(),
        "writable mounts must be dropped in read-only mode"
    );
    assert_eq!(
        narrowed.readable_mounts,
        vec![("/home/x/.rustup".to_owned(), "/home/x/.rustup".to_owned())],
        "readable mounts must be preserved"
    );
}

#[test]
fn read_only_sandbox_follows_main_network_config() {
    let mut sandbox = crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: vec!["/mnt/big/cargo-home".into()],
        readable_paths: vec!["~/.rustup".into()],
        readable_mounts: Vec::new(),
        writable_mounts: Vec::new(),
    };
    // Main config network = true → the read-only role keeps networking.
    let narrowed = read_only_sandbox(&sandbox);
    assert!(
        narrowed.network,
        "network=true in the main config must carry over to the read-only sandbox"
    );
    // Main config network = false → the read-only role is offline too.
    sandbox.network = false;
    let narrowed = read_only_sandbox(&sandbox);
    assert!(
        !narrowed.network,
        "network=false in the main config must carry over to the read-only sandbox"
    );
    // The read-only guarantees hold either way.
    assert!(!narrowed.workspace_writable, "workspace must be read-only");
    assert!(
        narrowed.writable_paths.is_empty(),
        "extra writable roots must be dropped"
    );
    assert_eq!(
        narrowed.readable_paths,
        vec!["~/.rustup".to_owned()],
        "readable roots must be preserved"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn background_bash_uses_the_facade_sandbox_not_the_registry_one() {
    // Regression: a background command must run under THIS Bash facade's
    // sandbox (a read-only role's narrowed policy), never the shared
    // registry's wider one — otherwise a read-only subagent's background
    // bash could write to the workspace.
    let Some(mut registry_sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    // The registry (parent) policy is deliberately writable + networked.
    registry_sandbox.workspace_writable = true;
    registry_sandbox.network = true;
    let narrowed = read_only_sandbox(&registry_sandbox);

    let temp = tempfile::tempdir().unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut bash = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(30)),
        sender: None,
        background: BackgroundTasks::new(
            Some(Duration::from_secs(30 * 60)),
            Some(registry_sandbox),
        ),
        sandbox: Some(narrowed),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    bash.set_event_sender(sender.clone());
    bash.background.set_event_sender(sender);

    let started = bash
        .execute(json!({"command": "touch escape.txt", "background": true}))
        .await
        .unwrap()
        .content;
    assert!(started.starts_with("started background task"), "{started}");

    let event = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .expect("timed out waiting for the background completion")
        .unwrap();
    let AgentEvent::BackgroundCompleted { output, .. } = event else {
        panic!("expected BackgroundCompleted");
    };
    assert!(
        !temp.path().join("escape.txt").exists(),
        "background bash escaped the facade's read-only sandbox; output: {output}"
    );
}

fn background_bash(
    temp: &tempfile::TempDir,
    timeout: Duration,
) -> (Bash, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut bash = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(30)),
        sender: None,
        background: BackgroundTasks::new(Some(timeout), None),
        sandbox: None,
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    bash.set_event_sender(sender.clone());
    // This helper also exercises BackgroundTasks directly in a few tests.
    // Registry-origin work therefore uses the same test receiver.
    bash.background.set_event_sender(sender);
    (bash, receiver)
}

fn sandbox() -> Option<crate::config::Sandbox> {
    // Only run when bwrap is actually installed and user namespaces work.
    bwrap_available().then(|| crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: Vec::new(),
        readable_paths: Vec::new(),
        readable_mounts: Vec::new(),
        writable_mounts: Vec::new(),
    })
}

#[tokio::test]
async fn sandbox_allows_workspace_writes_but_not_outside() {
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // Writing inside the workspace succeeds.
    tool.execute(json!({"command": "echo hi > inside.txt"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("inside.txt")).unwrap(),
        "hi\n"
    );
    // Writing outside the workspace (/tmp is a fresh tmpfs, /usr is ro)
    // must not touch the host: /usr is read-only inside the sandbox.
    let result = tool
        .execute(json!({"command": "touch /usr/e_agent_sandbox_escape 2>&1"}))
        .await;
    assert!(result.is_err(), "write to /usr should fail inside sandbox");
    assert!(!std::path::Path::new("/usr/e_agent_sandbox_escape").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_mounts_configured_dests_at_their_configured_paths() {
    // The symlink-mount fix: a (canonical source, configured dest) pair in
    // readable_mounts/writable_mounts must appear INSIDE the sandbox at the
    // configured dest — even when the dest does not exist on the host and
    // would otherwise be shadowed by a fresh tmpfs (the ~/.cargo scenario:
    // the sandbox /home is a tmpfs, so the configured ~/.cargo mount point
    // only exists because the mount loop binds it).
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("ws");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    // Host roots whose content must appear under the configured dests.
    let readable_target = temp.path().join("cargo-home");
    let writable_target = temp.path().join("data-home");
    std::fs::create_dir_all(&readable_target).unwrap();
    std::fs::create_dir_all(&writable_target).unwrap();
    std::fs::write(readable_target.join("readable-file"), "readable-content").unwrap();
    let readable_canonical = std::fs::canonicalize(&readable_target).unwrap();
    let writable_canonical = std::fs::canonicalize(&writable_target).unwrap();
    let readable_dest = temp.path().join(".cargo");
    let writable_dest = temp.path().join(".data");
    let sandbox = crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: Vec::new(),
        readable_paths: Vec::new(),
        readable_mounts: vec![(
            readable_canonical.to_str().unwrap().to_owned(),
            readable_dest.to_str().unwrap().to_owned(),
        )],
        writable_mounts: vec![(
            writable_canonical.to_str().unwrap().to_owned(),
            writable_dest.to_str().unwrap().to_owned(),
        )],
    };
    let tool = Bash {
        workspace: Workspace::new(&workspace_dir).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // The configured readable dest is visible inside the sandbox with the
    // canonical source's content, and it is read-only.
    let result = tool
        .execute(json!({"command": format!(
            "cat {rd}/readable-file && ! touch {rd}/blocked 2>/dev/null && echo READONLY_OK",
            rd = readable_dest.display()
        )}))
        .await
        .unwrap()
        .content;
    assert!(result.contains("readable-content"), "{result}");
    assert!(result.contains("READONLY_OK"), "{result}");
    // The configured writable dest writes through to the canonical source.
    let result = tool
        .execute(json!({"command": format!(
            "echo written > {wd}/file && cat {wd}/file",
            wd = writable_dest.display()
        )}))
        .await
        .unwrap()
        .content;
    assert!(result.contains("written"), "{result}");
    assert_eq!(
        std::fs::read_to_string(writable_target.join("file")).unwrap(),
        "written\n"
    );
}

#[tokio::test]
async fn sandbox_can_disable_network() {
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    sandbox.network = false;
    let temp = tempfile::tempdir().unwrap();
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // No loopback either in a fresh net namespace: connecting anywhere fails.
    let result = tool
        .execute(
            json!({"command": "exec 3<>/dev/tcp/127.0.0.1/80 && echo NET_OK || echo NET_BLOCKED"}),
        )
        .await
        .unwrap()
        .content;
    assert!(result.contains("NET_BLOCKED"), "{result}");
}

#[tokio::test]
async fn sandbox_read_only_workspace_rejects_bash_and_file_tool_writes() {
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    sandbox.workspace_writable = false;
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("existing"), "data").unwrap();
    let workspace = Workspace::new(temp.path())
        .unwrap()
        .with_external_roots(&sandbox)
        .unwrap();
    // With the sandbox enabled and the workspace read-only, the file tools
    // are denied writes too (reads stay allowed).
    assert!(
        WriteFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": "file-tool", "content": "yes"}))
        .await
        .is_err()
    );
    assert!(!temp.path().join("file-tool").exists());
    assert!(
        EditFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": "existing", "old": "data", "new": "x"}))
        .await
        .is_err()
    );
    assert!(
        ReadFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": "existing"}))
        .await
        .unwrap()
        .content
        .contains("data")
    );
    let tool = Bash {
        workspace,
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    assert!(
        tool.execute(json!({"command": "touch bash-file"}))
            .await
            .is_err()
    );
    assert!(!temp.path().join("bash-file").exists());
}

#[tokio::test]
async fn file_tools_resolve_configured_alias_destinations() {
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let source = temp.path().join("source");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&workspace_dir).unwrap();
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), "one two one").unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![source.to_str().unwrap().to_owned()],
        writable_mounts: vec![(
            source.to_str().unwrap().to_owned(),
            alias.to_str().unwrap().to_owned(),
        )],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // read_file through the configured alias destination.
    assert!(
        ReadFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": alias.join("file")}))
        .await
        .unwrap()
        .content
        .contains("one two one")
    );
    // write_file through the alias writes through to the canonical source.
    WriteFile {
        workspace: workspace.clone(),
    }
    .execute(json!({"path": alias.join("new"), "content": "created"}))
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(source.join("new")).unwrap(),
        "created"
    );
    // edit_file through the alias edits the canonical source.
    EditFile {
        workspace: workspace.clone(),
    }
    .execute(json!({"path": alias.join("file"), "old": "two", "new": "x"}))
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(source.join("file")).unwrap(),
        "one x one"
    );
}

#[tokio::test]
async fn sandbox_ro_exact_file_child_denies_tool_writes_but_allows_siblings() {
    // Winner-before-mode at the tool level: with the sandbox enabled and a
    // writable workspace, an exact RO file child denies write_file and
    // edit_file (read_file still works), while a sibling follows the
    // workspace's own writable policy.
    let _guard = undo_test_guard().await;
    let temp = tempfile::tempdir().unwrap();
    let exact = temp.path().join("secret.txt");
    std::fs::write(&exact, "data").unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: true,
        readable_paths: vec![exact.to_str().unwrap().to_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(temp.path())
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // write_file and edit_file on the exact RO file are denied.
    assert!(
        WriteFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": "secret.txt", "content": "no"}))
        .await
        .is_err()
    );
    assert!(
        EditFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": "secret.txt", "old": "data", "new": "x"}))
        .await
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(&exact).unwrap(), "data");
    // read_file still works through the same winner.
    assert!(
        ReadFile {
            workspace: workspace.clone(),
        }
        .execute(json!({"path": "secret.txt"}))
        .await
        .unwrap()
        .content
        .contains("data")
    );
    // A sibling follows the RW workspace policy.
    WriteFile { workspace }
        .execute(json!({"path": "other.txt", "content": "yes"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("other.txt")).unwrap(),
        "yes"
    );
}

#[tokio::test]
async fn sandbox_workspace_mount_wins_over_external_ancestor() {
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let parent = tempfile::tempdir().unwrap();
    let workspace_dir = parent.path().join("workspace");
    std::fs::create_dir(&workspace_dir).unwrap();

    sandbox.workspace_writable = false;
    sandbox.writable_paths = vec![parent.path().to_str().unwrap().to_owned()];
    let read_only = Bash {
        workspace: Workspace::new(&workspace_dir).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox.clone()),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    assert!(
        read_only
            .execute(json!({"command": "touch must-not-write"}))
            .await
            .is_err()
    );
    assert!(!workspace_dir.join("must-not-write").exists());

    sandbox.workspace_writable = true;
    sandbox.writable_paths.clear();
    sandbox.readable_paths = vec![parent.path().to_str().unwrap().to_owned()];
    let writable = Bash {
        workspace: Workspace::new(&workspace_dir).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    writable
        .execute(json!({"command": "touch workspace-wins"}))
        .await
        .unwrap();
    assert!(workspace_dir.join("workspace-wins").exists());
}

#[tokio::test]
async fn sandbox_read_only_workspace_allows_explicit_writable_child() {
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let workspace_dir = tempfile::tempdir().unwrap();
    let child = workspace_dir.path().join("child");
    std::fs::create_dir(&child).unwrap();
    sandbox.workspace_writable = false;
    sandbox.writable_paths = vec![child.to_str().unwrap().to_owned()];
    let tool = Bash {
        workspace: Workspace::new(workspace_dir.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    tool.execute(json!({"command": "touch child/allowed"}))
        .await
        .unwrap();
    assert!(
        tool.execute(json!({"command": "touch denied"}))
            .await
            .is_err()
    );
    assert!(child.join("allowed").exists());
    assert!(!workspace_dir.path().join("denied").exists());
}

#[tokio::test]
async fn sandbox_reroot_keeps_startup_policy_anchor_read_only() {
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let parent = tempfile::tempdir().unwrap();
    let child = parent.path().join("child");
    let policy_dir = parent.path().join(".e-agent");
    std::fs::create_dir(&child).unwrap();
    std::fs::create_dir(&policy_dir).unwrap();
    let policy_path = policy_dir.join("config.toml");
    std::fs::write(&policy_path, "[sandbox]\n").unwrap();
    sandbox.writable_paths = vec![parent.path().to_str().unwrap().to_owned()];
    let workspace = Workspace::new(parent.path())
        .unwrap()
        .with_external_roots(&sandbox)
        .unwrap()
        .reroot(&child)
        .unwrap();
    let tool = Bash {
        workspace,
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    assert!(
        tool.execute(json!({"command": "echo no > ../.e-agent/config.toml"}))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(policy_path).unwrap(), "[sandbox]\n");
}

#[tokio::test]
async fn sandbox_missing_policy_cannot_be_created_through_writable_e_agent_child() {
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let policy_dir = workspace.path().join(".e-agent");
    std::fs::create_dir(&policy_dir).unwrap();
    sandbox.workspace_writable = false;
    sandbox.writable_paths = vec![policy_dir.to_str().unwrap().to_owned()];
    let tool = Bash {
        workspace: Workspace::new(workspace.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    assert!(
        tool.execute(json!({"command": "touch .e-agent/config.toml"}))
            .await
            .is_err()
    );
    assert!(!policy_dir.join("config.toml").exists());
}

/// Sandboxed Bash with a temp workspace and the given policy, for the
/// policy-anchor projection tests below.
#[cfg(unix)]
fn policy_bash(workspace: Workspace, sandbox: crate::config::Sandbox) -> Bash {
    Bash {
        workspace,
        timeout: Some(Duration::from_secs(20)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_keeps_existing_worktrees_writable() {
    // GreptimeDB reproduction: `.e-agent/` exists with `worktrees/`,
    // `config.toml` missing, workspace writable. The old implementation
    // froze the whole `.e-agent` dir read-only, breaking the session
    // backend's writes; the projection must keep existing top-level
    // ordinary entries writable per the final mount policy AND persistent
    // on the host, while the missing config stays ENOENT.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    let worktrees = ea.join("worktrees");
    std::fs::create_dir_all(&worktrees).unwrap();
    std::fs::write(worktrees.join("policy-anchor-fd"), "hello\n").unwrap();
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox().unwrap());
    // Existing top-level sibling: writable per policy and host-persistent.
    let out = tool
        .execute(json!({"command": "echo persisted > .e-agent/worktrees/policy-anchor-fd && cat .e-agent/worktrees/policy-anchor-fd"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("persisted"), "{out}");
    assert_eq!(
        std::fs::read_to_string(worktrees.join("policy-anchor-fd")).unwrap(),
        "persisted\n"
    );
    // The missing config file stays ENOENT and cannot be created.
    let denied = tool
        .execute(json!({"command": "touch .e-agent/config.toml 2>&1"}))
        .await;
    assert!(
        denied.is_err(),
        "config.toml must not be creatable inside the sandbox: {denied:?}"
    );
    assert!(!ea.join("config.toml").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_missing_config_stays_enoent_without_host_pollution() {
    // `.e-agent/` exists (empty) but `config.toml` is missing: the
    // projection is an empty read-only tmpfs, the config stays ENOENT and
    // cannot be created, and the host gains nothing (no empty file, no
    // stray entries).
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(&ea).unwrap();
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox().unwrap());
    let out = tool
        .execute(json!({"command": "cat .e-agent/config.toml 2>&1 || true"}))
        .await
        .unwrap()
        .content;
    assert!(
        out.contains("No such file"),
        "config.toml must be ENOENT inside the sandbox: {out}"
    );
    let denied = tool
        .execute(json!({"command": "touch .e-agent/config.toml 2>&1"}))
        .await;
    assert!(
        denied.is_err(),
        "config.toml must not be creatable: {denied:?}"
    );
    // No host pollution: the config file was never created and the existing
    // `.e-agent` directory gained nothing.
    assert!(!ea.join("config.toml").exists());
    let entries: Vec<_> = std::fs::read_dir(&ea).unwrap().collect();
    assert!(
        entries.is_empty(),
        "host .e-agent must stay untouched: {entries:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_absent_parent_no_host_pollution() {
    // No `.e-agent` at all on the host, writable workspace: the projection
    // is skipped (a tmpfs mountpoint there would be created through the
    // writable bind — host pollution — or fail EROFS under a read-only
    // one). The generic mount loop already excludes the subtree, so the
    // config stays ENOENT and the host gains nothing.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("existing"), "data\n").unwrap();
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox().unwrap());
    let out = tool
        .execute(json!({"command": "cat .e-agent/config.toml 2>&1 || true; cat existing"}))
        .await
        .unwrap()
        .content;
    assert!(
        out.contains("No such file"),
        "config.toml must be ENOENT inside the sandbox: {out}"
    );
    assert!(out.contains("data"), "{out}");
    assert!(
        !temp.path().join(".e-agent").exists(),
        "host must not gain an empty .e-agent directory"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_missing_top_level_sibling_fails() {
    // A missing top-level sibling of the policy file (a new `.e-agent`
    // entry) must fail to be created inside the sandbox.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".e-agent/worktrees")).unwrap();
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox().unwrap());
    let denied = tool
        .execute(json!({"command": "touch .e-agent/sessions.db 2>&1"}))
        .await;
    assert!(
        denied.is_err(),
        "a missing top-level .e-agent entry must not be creatable: {denied:?}"
    );
    assert!(!temp.path().join(".e-agent/sessions.db").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_symlinks_never_exposed_or_followed() {
    // `.e-agent/escape -> .` and a config.toml symlink pointing at a host
    // secret must neither be exposed nor followed: the projection hides
    // symlinks entirely (they are never projected, never resolved).
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(&ea).unwrap();
    symlink(".", ea.join("escape")).unwrap();
    let secret = temp.path().join("secret.txt");
    std::fs::write(&secret, "TOP-SECRET\n").unwrap();
    symlink(&secret, ea.join("config.toml")).unwrap();
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox().unwrap());
    // Neither symlink is projected as an entry.
    let listing = tool
        .execute(json!({"command": "ls -a .e-agent"}))
        .await
        .unwrap()
        .content;
    assert!(
        !listing.contains("escape") && !listing.contains("config.toml"),
        "symlinks must be hidden by the projection: {listing}"
    );
    // Following the symlink must fail, not expose the secret or traverse.
    let out = tool
        .execute(json!({"command": "cat .e-agent/config.toml 2>&1; cat .e-agent/escape/config.toml 2>&1; true"}))
        .await
        .unwrap()
        .content;
    assert!(
        !out.contains("TOP-SECRET"),
        "a symlinked config.toml must not expose its target: {out}"
    );
    assert!(
        out.contains("No such file"),
        "symlink paths must fail with ENOENT: {out}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_special_files_not_projected() {
    // FIFO/socket top-level entries must not be projected (hidden), and the
    // enumeration must not block on a FIFO.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(&ea).unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        ea.join("pipe").as_path(),
        rustix::fs::Mode::from_bits(0o600).unwrap(),
    )
    .unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(ea.join("sock")).unwrap();
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox().unwrap());
    let out = tool
        .execute(json!({"command": "ls -a .e-agent"}))
        .await
        .unwrap()
        .content;
    assert!(
        !out.contains("pipe") && !out.contains("sock"),
        "special files must be hidden by the projection: {out}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_explicit_ro_and_rw_children() {
    // Explicit descendants under `.e-agent`: an RO child stays read-only
    // and an RW child stays writable, per the final mount policy.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(&ea).unwrap();
    std::fs::write(ea.join("ro-child"), "locked\n").unwrap();
    std::fs::write(ea.join("rw-child"), "initial\n").unwrap();
    let mut sandbox = sandbox().unwrap();
    sandbox.readable_paths = vec![ea.join("ro-child").to_str().unwrap().to_owned()];
    sandbox.writable_paths = vec![ea.join("rw-child").to_str().unwrap().to_owned()];
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox);
    // RO child: readable, not writable.
    let out = tool
        .execute(json!({"command": "cat .e-agent/ro-child"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("locked"), "{out}");
    let denied = tool
        .execute(json!({"command": "echo x > .e-agent/ro-child 2>&1"}))
        .await;
    assert!(
        denied.is_err(),
        "explicit RO child must reject writes: {denied:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ea.join("ro-child")).unwrap(),
        "locked\n"
    );
    // RW child: writable and host-persistent.
    tool.execute(json!({"command": "echo yes > .e-agent/rw-child"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(ea.join("rw-child")).unwrap(),
        "yes\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_writable_worktrees_with_read_only_workspace() {
    // workspace_writable=false stays unchanged: the workspace is read-only,
    // but an explicit writable child inside `.e-agent` remains writable and
    // the config file still cannot be created.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(ea.join("worktrees")).unwrap();
    let mut sandbox = sandbox().unwrap();
    sandbox.workspace_writable = false;
    sandbox.writable_paths = vec![ea.join("worktrees").to_str().unwrap().to_owned()];
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox);
    tool.execute(json!({"command": "echo ok > .e-agent/worktrees/w"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(ea.join("worktrees/w")).unwrap(),
        "ok\n"
    );
    // The workspace itself stays read-only.
    let denied = tool
        .execute(json!({"command": "touch top-level-file 2>&1"}))
        .await;
    assert!(denied.is_err(), "workspace must stay read-only: {denied:?}");
    assert!(!temp.path().join("top-level-file").exists());
    // The config file still cannot be created.
    let denied = tool
        .execute(json!({"command": "touch .e-agent/config.toml 2>&1"}))
        .await;
    assert!(
        denied.is_err(),
        "config.toml must not be creatable: {denied:?}"
    );
    assert!(!ea.join("config.toml").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_mount_aliases_cannot_bypass_config() {
    // A writable mount directly onto the policy parent and a readable mount
    // onto the policy file must not project their content over the protected
    // entries, and the writable mount must not allow creating the config.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(ea.join("worktrees")).unwrap();
    std::fs::write(ea.join("worktrees/w"), "real\n").unwrap();
    let decoy = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(decoy.path().join("worktrees")).unwrap();
    std::fs::write(decoy.path().join("config.toml"), "decoy-policy\n").unwrap();
    std::fs::write(decoy.path().join("worktrees/w"), "decoy\n").unwrap();
    let mut sandbox = sandbox().unwrap();
    sandbox.writable_mounts = vec![(
        decoy.path().to_str().unwrap().to_owned(),
        ea.to_str().unwrap().to_owned(),
    )];
    sandbox.readable_mounts = vec![(
        decoy.path().to_str().unwrap().to_owned(),
        ea.join("config.toml").to_str().unwrap().to_owned(),
    )];
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox);
    // The real `.e-agent` content is restored, not the mount source's.
    let out = tool
        .execute(json!({"command": "cat .e-agent/worktrees/w 2>&1; ls -a .e-agent"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("real"), "{out}");
    assert!(
        !out.contains("decoy"),
        "mount aliases must not project over the policy parent: {out}"
    );
    // The config file stays ENOENT: the writable mount at `.e-agent` cannot
    // create it, and the readable mount onto the config is not created.
    let denied = tool
        .execute(json!({"command": "touch .e-agent/config.toml 2>&1"}))
        .await;
    assert!(
        denied.is_err(),
        "a writable mount alias must not bypass the config protection: {denied:?}"
    );
    assert!(!ea.join("config.toml").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_toctou_pin_binds_old_inode() {
    // Descriptor-pin the projection, THEN swap the pathnames underneath:
    // the sandbox must bind the old inodes, never the swapped-in ones.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    use std::io::Read as _;
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(ea.join("worktrees")).unwrap();
    std::fs::write(ea.join("worktrees/m"), "OLD\n").unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let sandbox = sandbox().unwrap();
    let root_str = workspace.root().to_string_lossy().into_owned();
    let plan =
        super::bash::build_bwrap_plan(&workspace, &sandbox, false, true, &root_str, None).unwrap();
    // Hostile swap after the pin.
    std::fs::rename(&ea, temp.path().join(".e-agent.old")).unwrap();
    std::fs::create_dir_all(ea.join("worktrees")).unwrap();
    std::fs::write(ea.join("worktrees/m"), "NEW\n").unwrap();
    let mut child = super::bash::plan_spawn(
        &plan,
        &[
            std::ffi::OsString::from("/bin/cat"),
            ea.join("worktrees/m").into_os_string(),
        ],
    )
    .unwrap();
    let mut output = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "plan spawn failed: {status:?}");
    assert_eq!(
        output, "OLD\n",
        "a post-pin pathname swap must not rebind the projection"
    );
    assert_eq!(
        std::fs::read_to_string(ea.join("worktrees/m")).unwrap(),
        "NEW\n",
        "the swapped-in host inode must stay untouched"
    );
    std::fs::remove_dir_all(temp.path().join(".e-agent.old")).ok();
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_non_utf8_names_projected_byte_exact() {
    // Non-UTF-8 names must be projected byte-exact, never lossy: the
    // sandbox resolves the exact byte name (bash ANSI-C quoting), which
    // only works if the projection preserved the raw name bytes.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(&ea).unwrap();
    std::fs::write(
        ea.join(std::ffi::OsStr::from_bytes(b"we\xffird.txt")),
        "byte-exact\n",
    )
    .unwrap();
    std::fs::create_dir(ea.join(std::ffi::OsStr::from_bytes(b"d\xfeir"))).unwrap();
    std::fs::write(
        ea.join(std::ffi::OsStr::from_bytes(b"d\xfeir")).join("f"),
        "nested\n",
    )
    .unwrap();
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox().unwrap());
    let out = tool
        .execute(json!({"command": "cat .e-agent/$'we\\xffird.txt'; cat .e-agent/$'d\\xfeir'/f"}))
        .await
        .unwrap()
        .content;
    assert!(
        out.contains("byte-exact") && out.contains("nested"),
        "non-UTF-8 names must be projected byte-exact: {out}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_execute_only_parent_fails_closed() {
    // An execute-only (mode 0100) policy parent cannot be opened O_RDONLY,
    // so the projection cannot enumerate and rebuild it — but the sandbox
    // (same user) can still open `config.toml` by known filename through
    // the writable workspace bind. Skipping the projection would silently
    // drop the protection, so plan construction must fail closed instead.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(&ea).unwrap();
    std::fs::write(ea.join("config.toml"), "[sandbox]\n").unwrap();
    std::fs::set_permissions(&ea, std::fs::Permissions::from_mode(0o100)).unwrap();
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox().unwrap());
    let result = tool.execute(json!({"command": "echo ran"})).await;
    // Restore permissions so tempdir cleanup can remove the tree.
    std::fs::set_permissions(&ea, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        result.is_err(),
        "an unreadable policy parent must fail closed, not run unprotected: {result:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ea.join("config.toml")).unwrap(),
        "[sandbox]\n",
        "the host config.toml must stay untouched"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_rerooted_worktree_git_stays_read_only() {
    // A subagent rerooted into `.e-agent/worktrees/<name>` has its `.git`
    // under the policy parent. The projection's `--tmpfs .e-agent` plus the
    // top-level writable `worktrees` bind would shadow a `.git` ro-bind
    // installed BEFORE the projection; protect_git must be applied AFTER it
    // so the nested git directory stays read-only.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().to_path_buf();
    let feature = parent.join(".e-agent/worktrees/feature");
    std::fs::create_dir_all(feature.join(".git")).unwrap();
    std::fs::write(feature.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let mut sandbox = sandbox().unwrap();
    sandbox.writable_paths = vec![parent.to_str().unwrap().to_owned()];
    let workspace = Workspace::new(&parent)
        .unwrap()
        .with_external_roots(&sandbox)
        .unwrap()
        .reroot(&feature)
        .unwrap();
    let tool = Bash {
        workspace,
        timeout: Some(Duration::from_secs(20)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: true,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // Reading git metadata works (git commands read it).
    let out = tool
        .execute(json!({"command": "cat .git/HEAD"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("ref: refs/heads/main"), "{out}");
    // Writing must fail even though the rerooted workspace is writable.
    let write = tool
        .execute(json!({"command": "echo corrupted > .git/HEAD 2>&1"}))
        .await;
    assert!(
        write.is_err(),
        "nested .git must stay read-only under the projection: {write:?}"
    );
    let rm = tool.execute(json!({"command": "rm -rf .git 2>&1"})).await;
    assert!(rm.is_err(), "rm -rf nested .git must fail: {rm:?}");
    // The host git directory must be untouched.
    assert_eq!(
        std::fs::read_to_string(feature.join(".git/HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_rerooted_worktree_git_pointer_stays_read_only() {
    // Same reroot scenario with a linked-worktree `.git` pointer FILE: the
    // pointer must stay read-only under the projection too.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().to_path_buf();
    let feature = parent.join(".e-agent/worktrees/feature");
    std::fs::create_dir_all(&feature).unwrap();
    std::fs::write(
        feature.join(".git"),
        "gitdir: /some/external/main/.git/worktrees/feature\n",
    )
    .unwrap();
    let mut sandbox = sandbox().unwrap();
    sandbox.writable_paths = vec![parent.to_str().unwrap().to_owned()];
    let workspace = Workspace::new(&parent)
        .unwrap()
        .with_external_roots(&sandbox)
        .unwrap()
        .reroot(&feature)
        .unwrap();
    let tool = Bash {
        workspace,
        timeout: Some(Duration::from_secs(20)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: true,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    let out = tool
        .execute(json!({"command": "cat .git"}))
        .await
        .unwrap()
        .content;
    assert!(
        out.contains("/some/external/main/.git/worktrees/feature"),
        "{out}"
    );
    let write = tool
        .execute(json!({"command": "echo 'gitdir: /evil' > .git 2>&1"}))
        .await;
    assert!(
        write.is_err(),
        "the nested .git pointer must stay read-only: {write:?}"
    );
    let rm = tool.execute(json!({"command": "rm -f .git 2>&1"})).await;
    assert!(rm.is_err(), "rm of the .git pointer must fail: {rm:?}");
    assert_eq!(
        std::fs::read_to_string(feature.join(".git")).unwrap(),
        "gitdir: /some/external/main/.git/worktrees/feature\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_rerooted_worktree_git_read_only_background_bash() {
    // Background bash inherits protect_git=true; the rerooted worktree's
    // nested .git must stay read-only there too (the projection shadows
    // pre-projection ro-binds in the background task the same way).
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().to_path_buf();
    let feature = parent.join(".e-agent/worktrees/feature");
    std::fs::create_dir_all(feature.join(".git")).unwrap();
    std::fs::write(feature.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let mut sandbox = sandbox().unwrap();
    sandbox.writable_paths = vec![parent.to_str().unwrap().to_owned()];
    let workspace = Workspace::new(&parent)
        .unwrap()
        .with_external_roots(&sandbox)
        .unwrap()
        .reroot(&feature)
        .unwrap();
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut bash = Bash {
        workspace,
        timeout: Some(Duration::from_secs(20)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30 * 60)), Some(sandbox.clone())),
        sandbox: Some(sandbox),
        protect_git: true,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    bash.set_event_sender(sender);
    let start = bash
        .execute(json!({"command": "echo corrupted > .git/HEAD 2>&1", "background": true}))
        .await
        .unwrap()
        .content;
    assert!(start.starts_with("started background task"), "{start}");
    // Give it time to run and fail.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        std::fs::read_to_string(feature.join(".git/HEAD")).unwrap(),
        "ref: refs/heads/main\n",
        "background bash must not corrupt the nested .git"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_descendants_depth_sorted_ro_parent_rw_child() {
    // Explicit descendants must bind ancestor-first, descendant-last:
    // `readable_paths=[.../cache/sub]` + `writable_paths=[.../cache/sub/build]`
    // collected in that group order would bind the deeper RW child first and
    // let the shallower RO parent stack over (shadow) it.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    std::fs::create_dir_all(ea.join("cache/sub/build")).unwrap();
    let mut sandbox = sandbox().unwrap();
    sandbox.readable_paths = vec![ea.join("cache/sub").to_str().unwrap().to_owned()];
    sandbox.writable_paths = vec![ea.join("cache/sub/build").to_str().unwrap().to_owned()];
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox);
    // The RW child under an RO parent is writable and host-persistent.
    let out = tool
        .execute(json!({"command": "echo yes > .e-agent/cache/sub/build/w && cat .e-agent/cache/sub/build/w"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("yes"), "{out}");
    assert_eq!(
        std::fs::read_to_string(ea.join("cache/sub/build/w")).unwrap(),
        "yes\n"
    );
    // The RO parent itself stays read-only.
    let denied = tool
        .execute(json!({"command": "touch .e-agent/cache/sub/denied 2>&1"}))
        .await;
    assert!(
        denied.is_err(),
        "the RO parent must reject writes: {denied:?}"
    );
    assert!(!ea.join("cache/sub/denied").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_descendants_depth_sorted_ro_parent_rw_child_mount_alias() {
    // Path/mount alias mix: a readable MOUNT alias onto the RO parent plus a
    // writable PATH child. Collection order (paths before mounts) alone
    // would bind the RW child first and let the RO alias parent shadow it.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ea = temp.path().join(".e-agent");
    // The mountpoint for the RO alias parent must already exist: bwrap
    // cannot mkdir through the tmpfs'd `.e-agent` to create it.
    std::fs::create_dir_all(ea.join("cache/sub/build")).unwrap();
    let ro_src = tempfile::tempdir().unwrap();
    std::fs::write(ro_src.path().join("locked"), "locked\n").unwrap();
    std::fs::create_dir(ro_src.path().join("build")).unwrap();
    let mut sandbox = sandbox().unwrap();
    sandbox.readable_mounts = vec![(
        ro_src.path().to_str().unwrap().to_owned(),
        ea.join("cache/sub").to_str().unwrap().to_owned(),
    )];
    sandbox.writable_paths = vec![ea.join("cache/sub/build").to_str().unwrap().to_owned()];
    let tool = policy_bash(Workspace::new(temp.path()).unwrap(), sandbox);
    // The RO alias parent shows the alias source, read-only.
    let out = tool
        .execute(json!({"command": "cat .e-agent/cache/sub/locked"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("locked"), "{out}");
    let denied = tool
        .execute(json!({"command": "touch .e-agent/cache/sub/denied 2>&1"}))
        .await;
    assert!(
        denied.is_err(),
        "the aliased RO parent must reject writes: {denied:?}"
    );
    // The RW child under the aliased RO parent stays writable: the child
    // bind must be installed after (stack above) the RO alias parent.
    let out = tool
        .execute(json!({"command": "echo yes > .e-agent/cache/sub/build/w && cat .e-agent/cache/sub/build/w"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("yes"), "{out}");
    assert_eq!(
        std::fs::read_to_string(ea.join("cache/sub/build/w")).unwrap(),
        "yes\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_descendant_source_swapped_to_fifo_fails_closed() {
    // Between config resolution and plan construction the configured
    // descendant source is replaced by a FIFO (or socket): O_PATH still
    // opens it, so the pinned fd must be fstat-checked — only regular
    // files/directories may be projected; anything else fails closed.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let build = |make_special: &dyn Fn(&std::path::Path)| {
        let temp = tempfile::tempdir().unwrap();
        let ea = temp.path().join(".e-agent");
        std::fs::create_dir_all(ea.join("cache")).unwrap();
        let pinned = ea.join("cache/pinned");
        std::fs::create_dir(&pinned).unwrap();
        let mut sandbox = sandbox().unwrap();
        sandbox.writable_paths = vec![pinned.to_str().unwrap().to_owned()];
        // Config resolution: capabilities are opened from the regular dir.
        let workspace = Workspace::new(temp.path())
            .unwrap()
            .with_external_roots(&sandbox)
            .unwrap();
        // Hostile swap BEFORE plan construction.
        std::fs::remove_dir(&pinned).unwrap();
        make_special(&pinned);
        let root_str = temp.path().to_string_lossy().into_owned();
        super::bash::build_bwrap_plan(&workspace, &sandbox, false, true, &root_str, None)
    };
    let fifo_err = match build(&|path| {
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            path,
            rustix::fs::Mode::from_bits(0o600).unwrap(),
        )
        .unwrap();
    }) {
        Ok(_) => panic!("a FIFO descendant source must fail closed"),
        Err(err) => err,
    };
    assert!(
        fifo_err.contains("regular file or directory"),
        "a FIFO descendant source must fail closed: {fifo_err}"
    );
    let sock_err = match build(&|path| {
        let _listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    }) {
        Ok(_) => panic!("a socket descendant source must fail closed"),
        Err(err) => err,
    };
    assert!(
        sock_err.contains("regular file or directory"),
        "a socket descendant source must fail closed: {sock_err}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_policy_anchor_filtered_alias_without_projection_fails_closed() {
    // A configured alias (canonical source != configured dest) whose dest
    // lies under the policy parent is filtered from the generic mount loop.
    // When the projection cannot run — the anchor is not visible from a
    // workspace rerooted into a descendant of the alias SOURCE — the
    // filtered destination would be silently hidden; the plan must fail
    // closed instead.
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("sub")).unwrap();
    std::fs::write(src.path().join("sub/data.txt"), "data\n").unwrap();
    std::fs::create_dir_all(parent.path().join(".e-agent")).unwrap();
    let mut sandbox = sandbox().unwrap();
    sandbox.writable_mounts = vec![(
        src.path().to_str().unwrap().to_owned(),
        parent
            .path()
            .join(".e-agent/cache")
            .to_str()
            .unwrap()
            .to_owned(),
    )];
    // Reroot into a descendant of the canonical SOURCE: the startup anchor
    // (`parent/.e-agent/config.toml`) is visible from neither the new root
    // nor any external canonical source, so the projection cannot run.
    let workspace = Workspace::new(parent.path())
        .unwrap()
        .with_external_roots(&sandbox)
        .unwrap()
        .reroot(src.path().join("sub"))
        .unwrap();
    let tool = policy_bash(workspace, sandbox);
    let result = tool.execute(json!({"command": "echo ok"})).await;
    let err = match result {
        Ok(_) => {
            panic!("a filtered policy-subtree alias without a projection must fail closed");
        }
        Err(err) => err,
    };
    assert!(
        err.contains("fail closed") && err.contains("policy"),
        "the refusal must explain the fail-closed policy conflict: {err}"
    );
}

#[tokio::test]
async fn sandbox_ro_parent_allows_rw_child_override() {
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let parent = tempfile::tempdir().unwrap();
    let child = parent.path().join("child");
    std::fs::create_dir(&child).unwrap();
    sandbox.readable_paths = vec![parent.path().to_str().unwrap().to_owned()];
    sandbox.writable_paths = vec![child.to_str().unwrap().to_owned()];
    let temp = tempfile::tempdir().unwrap();
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    tool.execute(json!({"command": format!("touch '{}/yes'", child.display())}))
        .await
        .unwrap();
    assert!(
        tool.execute(json!({"command": format!("touch '{}/no'", parent.path().display())}))
            .await
            .is_err()
    );
    assert!(child.join("yes").exists());
    assert!(!parent.path().join("no").exists());
}

#[tokio::test]
async fn sandbox_extra_writable_and_readable_paths() {
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    // A "cache" dir outside the workspace (like ~/.cargo or a shared
    // target disk) and a read-only data dir.
    let cache = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("info.txt"), "data").unwrap();
    sandbox.writable_paths = vec![cache.path().to_string_lossy().into_owned()];
    sandbox.readable_paths = vec![data.path().to_string_lossy().into_owned()];

    let temp = tempfile::tempdir().unwrap();
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    let cache_path = cache.path().to_string_lossy().into_owned();
    let data_path = data.path().to_string_lossy().into_owned();
    // Writable path: read + write.
    tool.execute(json!({"command": format!("echo cached > '{cache_path}/entry'")}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(cache.path().join("entry")).unwrap(),
        "cached\n"
    );
    // Readable path: readable but NOT writable.
    let out = tool
        .execute(json!({"command": format!("cat '{data_path}/info.txt'")}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("data"), "{out}");
    let write = tool
        .execute(json!({"command": format!("touch '{data_path}/nope' 2>&1")}))
        .await;
    assert!(write.is_err(), "readable path must reject writes");
    assert!(!data.path().join("nope").exists());
}

#[tokio::test]
async fn sandbox_protects_workspace_git_directory() {
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    // Create a workspace with a real-looking .git directory.
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".git")).unwrap();
    std::fs::write(temp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: true,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // Reading .git/HEAD must succeed (git commands read metadata).
    let out = tool
        .execute(json!({"command": "cat .git/HEAD"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("ref: refs/heads/main"), "{out}");
    // Writing to any file under .git must fail (read-only bind).
    let write = tool
        .execute(json!({"command": "echo corrupted > .git/HEAD 2>&1"}))
        .await;
    assert!(write.is_err(), "write to .git must be rejected: {write:?}");
    // Removing the .git directory must also fail.
    let rm = tool.execute(json!({"command": "rm -rf .git 2>&1"})).await;
    assert!(rm.is_err(), "rm -rf .git must be rejected: {rm:?}");
    // The host .git must be untouched.
    assert_eq!(
        std::fs::read_to_string(temp.path().join(".git/HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
}

#[tokio::test]
async fn sandbox_protects_workspace_git_file_linked_worktree() {
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    // Simulate a linked-worktree worktree: .git is a file containing
    // a gitdir pointer to the main repo (outside the sandbox).
    let temp = tempfile::tempdir().unwrap();
    let gitdir = "/some/external/main/.git/worktrees/feature";
    std::fs::write(temp.path().join(".git"), format!("gitdir: {gitdir}\n")).unwrap();
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: true,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // Reading the .git pointer must succeed.
    let out = tool
        .execute(json!({"command": "cat .git"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains(gitdir), "{out}");
    // Overwriting or deleting the .git file must fail.
    let write = tool
        .execute(json!({"command": "echo 'gitdir: /evil' > .git 2>&1"}))
        .await;
    assert!(
        write.is_err(),
        "overwrite .git pointer must be rejected: {write:?}"
    );
    let rm = tool.execute(json!({"command": "rm -f .git 2>&1"})).await;
    assert!(rm.is_err(), "rm .git pointer must be rejected: {rm:?}");
    // Verify the host .git pointer is intact.
    assert_eq!(
        std::fs::read_to_string(temp.path().join(".git")).unwrap(),
        format!("gitdir: {gitdir}\n")
    );
}

#[tokio::test]
async fn sandbox_mounts_systemd_resolve_when_present() {
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // Check whether /run/systemd/resolve exists on this host.
    let host_has_resolve = std::path::Path::new("/run/systemd/resolve").exists();
    // Inside the sandbox /run should be visible only when systemd-resolve
    // was mounted. The directory itself should either exist and be readable
    // or not exist at all.
    let out = tool
        .execute(json!({"command": "test -d /run/systemd/resolve && echo PRESENT || echo ABSENT"}))
        .await
        .unwrap()
        .content;
    if host_has_resolve {
        assert!(
            out.contains("PRESENT"),
            "/run/systemd/resolve should be mounted when host has it; output: {out}"
        );
        // The stub-resolv.conf should also be readable.
        let contents = tool
            .execute(json!({"command": "cat /run/systemd/resolve/stub-resolv.conf 2>&1 || true"}))
            .await
            .unwrap()
            .content;
        assert!(
            contents.contains("nameserver"),
            "stub-resolv.conf should contain a nameserver; output: {contents}"
        );
    } else {
        assert!(
            out.contains("ABSENT"),
            "/run/systemd/resolve should NOT exist when host lacks it; output: {out}"
        );
    }
}

#[tokio::test]
async fn sandbox_cat_etc_resolv_conf_works() {
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // /etc/resolv.conf is always mounted (--ro-bind-try). Its contents
    // depend on the host config; we just check it is readable.
    let out = tool
        .execute(json!({"command": "cat /etc/resolv.conf 2>&1 || true"}))
        .await
        .unwrap()
        .content;
    if std::path::Path::new("/etc/resolv.conf").exists() {
        // On hosts with systemd-resolved the symlink target may or may not
        // be reachable. We only assert the file itself is present (the
        // symlink target is covered by sandbox_mounts_systemd_resolve).
        assert!(
            !out.contains("No such file"),
            "/etc/resolv.conf should be readable; output: {out}"
        );
    } else {
        // Host without /etc/resolv.conf (unusual) – skip assertion.
        eprintln!("host has no /etc/resolv.conf; skipping content check");
    }
}

#[tokio::test]
async fn sandbox_dns_resolution_live_smoke() {
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(15)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // Live DNS resolution: this is a network-dependent smoke test.
    // Skip if /run/systemd/resolve does not exist on the host (no
    // systemd-resolved) or if basic connectivity prerequisites are
    // absent. Use getent hosts (glibc) which respects /etc/nsswitch.conf.
    let host_has_resolve = std::path::Path::new("/run/systemd/resolve").exists();
    if !host_has_resolve {
        eprintln!("host has no /run/systemd/resolve; skipping live DNS smoke test");
        return;
    }
    // First verify that the resolver stub is reachable inside the sandbox.
    let stub_ok = tool
        .execute(json!({"command": "cat /run/systemd/resolve/stub-resolv.conf 2>&1"}))
        .await;
    match stub_ok {
        Ok(out) if out.content.contains("nameserver") => { /* proceed */ }
        Ok(out) => {
            eprintln!(
                "stub-resolv.conf reachable but no nameserver line: {}",
                out.content
            );
            return;
        }
        Err(e) => {
            eprintln!("stub-resolv.conf not reachable inside sandbox: {e}");
            return;
        }
    }
    // Try resolving github.com (commonly available public host).
    let result = tool
            .execute(json!({"command": "getent hosts github.com 2>&1 || nslookup github.com 2>&1 || host github.com 2>&1 || dig +short github.com 2>&1"}))
            .await;
    match result {
        Ok(out) => {
            let trimmed = out.content.trim();
            if trimmed.is_empty()
                || trimmed.contains("not found")
                || trimmed.contains("NXDOMAIN")
                || trimmed.contains("SERVFAIL")
            {
                eprintln!(
                    "DNS resolution returned no result (external network issue): {}",
                    out.content
                );
            } else {
                assert!(
                    trimmed.contains("github.com") || trimmed.contains('.'),
                    "expected resolved address but got: {}",
                    out.content
                );
            }
        }
        Err(e) => {
            eprintln!("DNS resolution command failed (external network issue): {e}");
        }
    }
}

#[tokio::test]
async fn completed_background_tasks_leave_the_running_registry() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
    for id in 1..=8 {
        assert!(
            bash.execute(json!({"command": "true", "background": true}))
                .await
                .unwrap()
                .content
                .starts_with(&format!("started background task {id}:"))
        );
    }
    for _ in 0..8 {
        receiver.recv().await.unwrap();
    }
    assert!(bash.background.running().is_empty());
    assert!(
        bash.execute(json!({"command": "true", "background": true}))
            .await
            .unwrap()
            .content
            .starts_with("started background task 9:")
    );
}

/// Background bash trace metadata: exit 0 / exit 3 / signal-killed record
/// the structured exit_code/signal/status on the completion event, with a
/// positive duration and a present started_at. The model-visible output
/// text ("exit code: N" / "exit code: signal") is unchanged — the
/// structured fields ride along as metadata.
#[tokio::test]
async fn background_bash_completion_carries_exit_trace() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(30));

    // exit 0 → status "completed", exit_code 0, no signal.
    bash.execute(json!({"command": "true", "background": true}))
        .await
        .unwrap();
    match receiver.recv().await.unwrap() {
        AgentEvent::BackgroundCompleted {
            output,
            exit_code,
            signal,
            status,
            kind,
            started_at_ms,
            duration_ms,
            ..
        } => {
            assert!(output.starts_with("exit code: 0\n"), "{output}");
            assert_eq!(exit_code, Some(0));
            assert_eq!(signal, None);
            assert_eq!(status.as_deref(), Some("completed"));
            assert_eq!(kind.as_deref(), Some("bash"));
            assert!(started_at_ms.is_some(), "started_at_ms must be present");
            assert!(
                duration_ms.is_some_and(|ms| ms > 0),
                "duration_ms must be positive, got {duration_ms:?}"
            );
        }
        other => panic!("expected BackgroundCompleted, got {other:?}"),
    }

    // exit 3 → status "failed", exit_code 3.
    bash.execute(json!({"command": "exit 3", "background": true}))
        .await
        .unwrap();
    match receiver.recv().await.unwrap() {
        AgentEvent::BackgroundCompleted {
            output,
            exit_code,
            signal,
            status,
            kind,
            duration_ms,
            ..
        } => {
            assert!(output.starts_with("exit code: 3\n"), "{output}");
            assert_eq!(exit_code, Some(3));
            assert_eq!(signal, None);
            assert_eq!(status.as_deref(), Some("failed"));
            assert_eq!(kind.as_deref(), Some("bash"));
            assert!(duration_ms.is_some_and(|ms| ms > 0));
        }
        other => panic!("expected BackgroundCompleted, got {other:?}"),
    }

    // SIGTERM self-kill → status "killed", no exit code, signal name.
    bash.execute(json!({"command": "kill -TERM $$", "background": true}))
        .await
        .unwrap();
    match receiver.recv().await.unwrap() {
        AgentEvent::BackgroundCompleted {
            output,
            exit_code,
            signal,
            status,
            kind,
            ..
        } => {
            assert!(output.starts_with("exit code: signal\n"), "{output}");
            assert_eq!(exit_code, None, "signal death has no exit code");
            assert_eq!(signal.as_deref(), Some("SIGTERM"));
            assert_eq!(status.as_deref(), Some("killed"));
            assert_eq!(kind.as_deref(), Some("bash"));
        }
        other => panic!("expected BackgroundCompleted, got {other:?}"),
    }
}

#[tokio::test]
async fn background_timeout_is_delivered_as_completion() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_millis(50));
    bash.execute(json!({"command": "sleep 30 & echo $! > child.pid; wait", "background": true}))
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event,
        AgentEvent::BackgroundCompleted { output, .. } if output.contains("timed out")
    ));
    let pid = std::fs::read_to_string(temp.path().join("child.pid")).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !std::process::Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.trim())
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn background_without_timeout_runs_to_completion() {
    let temp = tempfile::tempdir().unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut bash = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(30)),
        sender: None,
        // None = no timeout: a command sleeping past any small budget must
        // still complete normally instead of being killed.
        background: BackgroundTasks::new(None, None),
        sandbox: None,
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    bash.set_event_sender(sender.clone());
    bash.background.set_event_sender(sender);
    bash.execute(json!({"command": "sleep 2; echo done", "background": true}))
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .expect("background task without timeout never completed")
        .unwrap();
    assert!(matches!(
        event,
        AgentEvent::BackgroundCompleted { output, .. }
            if output.contains("done") && !output.contains("timed out")
    ));
}

#[tokio::test]
async fn background_task_output_is_visible_while_running() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, _receiver) = background_bash(&temp, Duration::from_secs(10));
    bash.execute(json!({
        "command": "echo hello; sleep 30",
        "background": true
    }))
    .await
    .unwrap();
    let mut saw = String::new();
    for _ in 0..50 {
        let running = bash.background.running();
        assert_eq!(running.len(), 1);
        // The snapshot carries the FULL command, not the truncated label.
        assert_eq!(
            running[0].full_command.as_deref(),
            Some("echo hello; sleep 30")
        );
        saw = String::from_utf8_lossy(&running[0].output).into_owned();
        if saw.contains("hello") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(saw.contains("hello"), "output slot never filled: {saw:?}");
}

#[tokio::test]
async fn background_snapshot_keeps_full_command_beyond_truncated_label() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
    // Longer than the 100-char label preview: the label must be truncated
    // while full_command keeps the entire command for the detail view.
    let command = format!(
        "echo {}; sleep 30",
        "abcdefghijklmnopqrstuvwxyz0123456789".repeat(10)
    );
    assert!(command.len() > 100);
    bash.execute(json!({
        "command": command.clone(),
        "background": true
    }))
    .await
    .unwrap();
    let running = bash.background.running();
    assert_eq!(running.len(), 1);
    assert_ne!(
        running[0].label, command,
        "label must stay preview-truncated"
    );
    assert!(
        running[0].label.contains('\u{2026}'),
        "label shows ellipsis"
    );
    assert_eq!(running[0].full_command.as_deref(), Some(command.as_str()));
    bash.background.cancel(running[0].id);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
}

#[tokio::test]
async fn cancel_kills_the_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, _receiver) = background_bash(&temp, Duration::from_secs(30));
    bash.execute(json!({
        "command": "sleep 30 & echo $! > child.pid; wait",
        "background": true
    }))
    .await
    .unwrap();
    let id = bash.background.running()[0].id;
    for _ in 0..50 {
        if temp.path().join("child.pid").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid = std::fs::read_to_string(temp.path().join("child.pid")).unwrap();
    let label = bash.background.cancel(id).unwrap();
    assert!(label.contains("sleep 30"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(bash.background.running().is_empty());
    assert!(
        !std::process::Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.trim())
            .status()
            .unwrap()
            .success(),
        "background child survived cancel"
    );
}

#[tokio::test]
async fn get_background_tasks_lists_running_tasks() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
    let background = bash.background.clone();
    let tool = GetBackgroundTasks::new(background.clone(), None, None);

    // No tasks running initially.
    assert_eq!(
        tool.execute(json!({})).await.unwrap().content,
        "No background tasks running."
    );

    // Start a background task (id=1).
    bash.execute(json!({"command": "echo hello; sleep 30", "background": true}))
        .await
        .unwrap();
    let mut tasks = background.running();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, 1);
    assert_eq!(tasks[0].kind, "bash");

    // The tool reports the running task with correct id, label, and role,
    // plus the full command on its own continuation line (the label is the
    // same here because the command is short — see the long-command test
    // below for the truncated-label case).
    let output = tool.execute(json!({})).await.unwrap().content;
    assert_eq!(
        output,
        "1 background task(s) running:\n#1: echo hello; sleep 30 (bash)\n    command: echo hello; sleep 30"
    );

    // Start a second background task (id=2).
    bash.execute(json!({"command": "echo world; sleep 30", "background": true}))
        .await
        .unwrap();
    tasks = background.running();
    assert_eq!(tasks.len(), 2);

    // Both tasks display with their actual ids, not list positions.
    let output = tool.execute(json!({})).await.unwrap().content;
    assert_eq!(
        output,
        "2 background task(s) running:\n#1: echo hello; sleep 30 (bash)\n    command: echo hello; sleep 30\n#2: echo world; sleep 30 (bash)\n    command: echo world; sleep 30"
    );

    // Cancel the first task (id=1).
    background.cancel(1);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;

    // The remaining task (id=2) still shows as #2, NOT renumbered to #1.
    let output = tool.execute(json!({})).await.unwrap().content;
    assert_eq!(
        output,
        "1 background task(s) running:\n#2: echo world; sleep 30 (bash)\n    command: echo world; sleep 30"
    );

    // Cleanup.
    background.cancel(2);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
    assert_eq!(
        tool.execute(json!({})).await.unwrap().content,
        "No background tasks running."
    );
}

#[tokio::test]
async fn get_background_tasks_shows_full_command_beyond_truncated_label() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
    let background = bash.background.clone();
    let tool = GetBackgroundTasks::new(background.clone(), None, None);

    // A command longer than the 100-char label budget: the label is
    // preview-truncated, but the tool's `command:` line must carry the
    // UNTRUNCATED original so the agent/UI can act on the full command.
    let long = "cargo build --release --features very-long-feature-name-here \
                --target x86_64-unknown-linux-gnu --jobs 8 --verbose";
    assert!(
        long.chars().count() > 100,
        "test command must exceed the label budget"
    );
    bash.execute(json!({"command": long, "background": true}))
        .await
        .unwrap();

    let output = tool.execute(json!({})).await.unwrap().content;
    // First line keeps the truncated label (backward compatible), the
    // continuation line carries the full command verbatim.
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines[0], "1 background task(s) running:");
    let label_line = lines[1];
    assert!(
        label_line.starts_with("#1: ") && label_line.ends_with(" (bash)"),
        "entry header unchanged: {label_line}"
    );
    assert!(
        label_line.contains('\u{2026}'),
        "label must be truncated with an ellipsis: {label_line}"
    );
    assert!(
        label_line != format!("#1: {long} (bash)"),
        "label must not be the full command"
    );
    assert_eq!(
        lines[2],
        format!("    command: {long}"),
        "command line carries the untruncated original"
    );

    background.cancel(1);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
}

#[tokio::test]
async fn get_background_tasks_shows_delegate_tasks_as_delegate_not_bash() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, _receiver) = background_bash(&temp, Duration::from_secs(10));
    let background = bash.background.clone();

    let tool = GetBackgroundTasks::new(background.clone(), None, None);

    // Spawn a delegate-style task (no role, no output slot) via spawn().
    background
        .spawn(
            "search codebase".into(),
            None, // role
            None, // process_group
            || async { "done".into() },
        )
        .unwrap();

    let output = tool.execute(json!({})).await.unwrap().content;
    // Delegate tasks without a role show their kind ("delegate") instead
    // of being mislabeled as "bash".
    assert_eq!(
        output,
        "1 background task(s) running:\n#1: search codebase (delegate)"
    );

    let tasks = background.running();
    assert_eq!(tasks[0].kind, "delegate");
    // Delegate tasks have no full command (bash-only field).
    assert_eq!(tasks[0].full_command, None);
}

#[tokio::test]
async fn get_background_tasks_shows_roled_delegate_with_role_name() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, _receiver) = background_bash(&temp, Duration::from_secs(10));
    let background = bash.background.clone();

    let tool = GetBackgroundTasks::new(background.clone(), None, None);

    // A delegate with a known role (e.g. "explorer").
    background
        .spawn(
            "search the logs".into(),
            Some("explorer".into()),
            None,
            || async { "done".into() },
        )
        .unwrap();

    let output = tool.execute(json!({})).await.unwrap().content;
    assert_eq!(
        output,
        "1 background task(s) running:\n#1: search the logs (explorer)"
    );
}

#[tokio::test]
async fn subagent_background_tasks_are_scoped_for_listing_and_cancel() {
    let temp = tempfile::tempdir().unwrap();
    let (mut bash, _receiver) = background_bash(&temp, Duration::from_secs(30));
    let background = bash.background.clone();
    let self_session_id = "sub-own";

    // Parent delegate/self and a main/unknown task have no owner and are
    // hidden from every subagent, even when the delegate metadata guesses the
    // caller's session id.
    background
        .spawn_with_id(
            "parent delegate".into(),
            Some("fixer".into()),
            None,
            Some(TaskDisplayMeta {
                subagent_session_id: Some(self_session_id.into()),
                ..TaskDisplayMeta::default()
            }),
            new_exit_slot(),
            |_| {},
            || async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "done".into()
            },
        )
        .unwrap();
    background
        .spawn("main task".into(), None, None, || async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            "done".into()
        })
        .unwrap();

    // A sibling-owned bash task is also invisible, while this task is visible.
    bash.owner_session = Some("sub-sibling".into());
    bash.execute(json!({"command": "sleep 30", "background": true}))
        .await
        .unwrap();
    bash.owner_session = Some(self_session_id.into());
    bash.execute(json!({"command": "sleep 30", "background": true}))
        .await
        .unwrap();

    let tasks = background.running();
    assert_eq!(tasks.len(), 4);
    let own_id = tasks[3].id;
    let parent_id = tasks[0].id;
    let sibling_id = tasks[2].id;
    let main_id = tasks[1].id;

    let list = GetBackgroundTasks::new(background.clone(), Some(self_session_id.into()), None);
    let output = list.execute(json!({})).await.unwrap().content;
    assert!(output.contains(&format!("#{own_id}: sleep 30 (bash)")));
    assert!(!output.contains(&format!("#{parent_id}:")));
    assert!(!output.contains(&format!("#{sibling_id}:")));
    assert!(!output.contains(&format!("#{main_id}:")));

    let cancel = CancelBackgroundTask {
        background: background.clone(),
        self_session_id: Some(self_session_id.into()),
    };
    for id in [parent_id, sibling_id, main_id] {
        assert_eq!(
            cancel.execute(json!({"id": id})).await.unwrap_err(),
            format!("background task {id} is not running")
        );
        assert!(
            background.running().iter().any(|task| task.id == id),
            "denied cancellation must leave task {id} running"
        );
    }

    assert_eq!(
        cancel.execute(json!({"id": own_id})).await.unwrap().content,
        format!("cancelled background task {own_id}")
    );

    // Main callers retain unrestricted visibility and cancellation.
    let main_list = GetBackgroundTasks::new(background.clone(), None, None);
    let main_output = main_list.execute(json!({})).await.unwrap().content;
    assert!(main_output.contains(&format!("#{parent_id}: parent delegate")));
    assert!(main_output.contains(&format!("#{sibling_id}: sleep 30")));
    assert!(main_output.contains(&format!("#{main_id}: main task")));
    let main_cancel = CancelBackgroundTask {
        background: background.clone(),
        self_session_id: None,
    };
    for id in [parent_id, sibling_id, main_id] {
        assert_eq!(
            main_cancel
                .execute(json!({"id": id}))
                .await
                .unwrap()
                .content,
            format!("cancelled background task {id}")
        );
    }
    assert!(background.running().is_empty());
}

#[tokio::test]
async fn subagent_hidden_tasks_do_not_trigger_poll_guard() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, _receiver) = background_bash(&temp, Duration::from_secs(30));
    let background = bash.background.clone();
    background
        .spawn("hidden global task".into(), None, None, || async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            "done".into()
        })
        .unwrap();
    let tool = GetBackgroundTasks::new(
        background.clone(),
        Some("sub-no-tasks".into()),
        Some(SUBAGENT_POLL_GUARD_THRESHOLD),
    );
    for _ in 0..10 {
        assert_eq!(
            tool.execute(json!({})).await.unwrap().content,
            "No background tasks running."
        );
    }
    for task in background.running() {
        background.cancel(task.id);
    }
}

#[tokio::test]
async fn subagent_poll_guard_never_escalates_on_empty_snapshots() {
    // Empty polls never escalate, no matter how often repeated: with no
    // running tasks there is nothing to wait for. 10 polls (well past the
    // subagent threshold 3) all return the plain output — no reminder, no
    // sentinel.
    let tool = GetBackgroundTasks::new(
        BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        None,
        Some(SUBAGENT_POLL_GUARD_THRESHOLD),
    );
    for _ in 0..10 {
        assert_eq!(
            tool.execute(json!({})).await.unwrap().content,
            "No background tasks running."
        );
    }
}

#[tokio::test]
async fn main_poll_guard_never_escalates_on_empty_snapshots() {
    // Same for the main-agent guard (threshold 5): 10 empty polls never
    // produce the reminder or the sentinel.
    let tool = GetBackgroundTasks::new(
        BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        None,
        Some(MAIN_POLL_GUARD_THRESHOLD),
    );
    for _ in 0..10 {
        assert_eq!(
            tool.execute(json!({})).await.unwrap().content,
            "No background tasks running."
        );
    }
}

#[tokio::test]
async fn subagent_poll_guard_escalates_on_unchanged_snapshot_and_resets_on_change() {
    // The subagent-only guard on a REAL non-empty snapshot: 1st poll is
    // normal, the 2nd is the model-facing POLL_ERROR, the 3rd is the
    // internal termination sentinel. Any ID-set change (new task,
    // cancellation, completion), an interleaved empty poll, and the
    // per-turn hook reset the count.
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
    let background = bash.background.clone();
    let mut tool = GetBackgroundTasks::new(
        background.clone(),
        None,
        Some(SUBAGENT_POLL_GUARD_THRESHOLD),
    );

    // A real task snapshot: 1st normal, 2nd reminder, 3rd termination.
    bash.execute(json!({"command": "echo hello; sleep 30", "background": true}))
        .await
        .unwrap();
    assert_eq!(background.running().len(), 1);
    let output = tool.execute(json!({})).await.unwrap().content;
    assert!(output.starts_with("1 background task(s) running:"));
    let warning = tool.execute(json!({})).await.unwrap_err();
    assert!(warning.starts_with("1 background task(s) running:"));
    assert!(warning.contains("#1: "));
    assert!(warning.contains(crate::agent::POLL_GUARD_ERROR));
    let warning = tool.execute(json!({})).await.unwrap_err();
    assert!(warning.starts_with("1 background task(s) running:"));
    assert!(warning.contains("#1: "));
    assert!(warning.ends_with(crate::agent::POLL_GUARD_SENTINEL));
    assert!(warning.contains(crate::agent::POLL_GUARD_TERMINATION_NOTICE));

    // An empty poll (task cancelled) clears the guard: repeated empty
    // polls never escalate…
    background.cancel(1);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
    for _ in 0..10 {
        assert_eq!(
            tool.execute(json!({})).await.unwrap().content,
            "No background tasks running."
        );
    }

    // …and a later real snapshot starts counting fresh: 1st normal, 2nd
    // POLL_ERROR — NOT the sentinel.
    bash.execute(json!({"command": "echo again; sleep 30", "background": true}))
        .await
        .unwrap();
    assert_eq!(background.running().len(), 1);
    let output = tool.execute(json!({})).await.unwrap().content;
    assert!(
        output.starts_with("1 background task(s) running:"),
        "empty poll must reset the guard: {output}"
    );
    let warning = tool.execute(json!({})).await.unwrap_err();
    assert!(warning.starts_with("1 background task(s) running:"));
    assert!(warning.contains(crate::agent::POLL_GUARD_ERROR));

    // Turn hook: a fresh true turn resets even with the same snapshot.
    tool.on_turn_start();
    let output = tool.execute(json!({})).await.unwrap().content;
    assert!(output.starts_with("1 background task(s) running:"));

    // Different subagent instances own their tools: independent counts.
    background.cancel(2);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
    bash.execute(json!({"command": "sleep 30", "background": true}))
        .await
        .unwrap();
    let tool_a = GetBackgroundTasks::new(
        background.clone(),
        None,
        Some(SUBAGENT_POLL_GUARD_THRESHOLD),
    );
    let tool_b = GetBackgroundTasks::new(
        background.clone(),
        None,
        Some(SUBAGENT_POLL_GUARD_THRESHOLD),
    );
    let output = tool_a.execute(json!({})).await.unwrap().content;
    assert!(output.starts_with("1 background task(s) running:"));
    let warning = tool_a.execute(json!({})).await.unwrap_err();
    assert!(warning.starts_with("1 background task(s) running:"));
    assert!(warning.contains(crate::agent::POLL_GUARD_ERROR));
    let output = tool_b.execute(json!({})).await.unwrap().content;
    assert!(output.starts_with("1 background task(s) running:"));
    let warning = tool_a.execute(json!({})).await.unwrap_err();
    assert!(warning.starts_with("1 background task(s) running:"));
    assert!(warning.ends_with(crate::agent::POLL_GUARD_SENTINEL));
    assert!(warning.contains(crate::agent::POLL_GUARD_TERMINATION_NOTICE));
}
#[tokio::test]
async fn subagent_poll_guard_completion_resets_and_output_growth_does_not() {
    // A task COMPLETING changes the ID set → reset. Output growth of the
    // same task never resets (only the sorted ID set is compared).
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
    let background = bash.background.clone();
    let tool = GetBackgroundTasks::new(
        background.clone(),
        None,
        Some(SUBAGENT_POLL_GUARD_THRESHOLD),
    );

    // Short task: completes on its own (~0.2s), spooling output over time.
    bash.execute(json!({"command": "echo one; sleep 0.2; echo two; sleep 30", "background": true}))
        .await
        .unwrap();
    assert_eq!(background.running().len(), 1);

    // 1st poll: normal.
    let output = tool.execute(json!({})).await.unwrap().content;
    assert!(output.starts_with("1 background task(s) running:"));

    // Let the task's spool grow (echo two lands ~0.2s after start): the ID
    // set is unchanged, so output growth must NOT reset — the 2nd poll is
    // still the POLL_ERROR.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        background
            .output(1)
            .is_some_and(|bytes| bytes.windows(3).any(|window| window == b"two")),
        "background task output must grow for the non-reset proof"
    );
    let warning = tool.execute(json!({})).await.unwrap_err();
    assert!(warning.starts_with("1 background task(s) running:"));
    assert!(warning.contains("#1: "));
    assert!(warning.contains(crate::agent::POLL_GUARD_ERROR));

    // A task COMPLETING changes the ID set → reset (normal poll again).
    // A fresh SHORT task gives a fresh snapshot {2}; after it completes the
    // observed set is empty — different from {2}, so the next poll resets.
    background.cancel(1);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
    bash.execute(json!({"command": "sleep 0.1", "background": true}))
        .await
        .unwrap();
    assert!(
        tool.execute(json!({}))
            .await
            .unwrap()
            .content
            .starts_with("1 background task(s) running:")
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), receiver.recv()).await;
    assert_eq!(
        tool.execute(json!({})).await.unwrap().content,
        "No background tasks running."
    );
}

#[tokio::test]
async fn main_poll_guard_escalates_1_2_normal_3_4_reminder_5_sentinel() {
    // The main-agent guard (5th-poll termination threshold): the 1st and
    // 2nd unchanged-snapshot polls of a REAL task are normal, the 3rd and
    // 4th return the model-facing POLL_ERROR, the 5th returns the internal
    // termination sentinel (softer than the subagent 2/3 escalation).
    // Empty snapshots never escalate, clear the guard, and the per-turn
    // hook resets the count.
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
    let background = bash.background.clone();
    let mut tool =
        GetBackgroundTasks::new(background.clone(), None, Some(MAIN_POLL_GUARD_THRESHOLD));

    // Empty polls never escalate (10 polls, well past the threshold)…
    for _ in 0..10 {
        assert_eq!(
            tool.execute(json!({})).await.unwrap().content,
            "No background tasks running."
        );
    }
    // …and the per-turn hook leaves that untouched.
    tool.on_turn_start();
    assert_eq!(
        tool.execute(json!({})).await.unwrap().content,
        "No background tasks running."
    );

    // A REAL task snapshot escalates: 1st and 2nd normal, 3rd and 4th
    // reminder, 5th termination; every warning retains the snapshot.
    bash.execute(json!({"command": "echo hello; sleep 30", "background": true}))
        .await
        .unwrap();
    assert_eq!(background.running().len(), 1);
    let output = tool.execute(json!({})).await.unwrap().content;
    assert!(
        output.starts_with("1 background task(s) running:"),
        "first poll of a real snapshot is normal: {output}"
    );
    let output = tool.execute(json!({})).await.unwrap().content;
    assert!(output.starts_with("1 background task(s) running:"));
    for _ in 0..2 {
        let warning = tool.execute(json!({})).await.unwrap_err();
        assert!(warning.starts_with("1 background task(s) running:"));
        assert!(warning.contains("#1: "));
        assert!(warning.contains(crate::agent::POLL_GUARD_ERROR));
    }
    let warning = tool.execute(json!({})).await.unwrap_err();
    assert!(warning.starts_with("1 background task(s) running:"));
    assert!(warning.contains("#1: "));
    assert!(warning.ends_with(crate::agent::POLL_GUARD_SENTINEL));
    assert!(warning.contains(crate::agent::POLL_GUARD_TERMINATION_NOTICE));

    // Turn hook: a fresh true turn resets even with the same snapshot.
    tool.on_turn_start();
    let output = tool.execute(json!({})).await.unwrap().content;
    assert!(output.starts_with("1 background task(s) running:"));

    // Cancelling empties the snapshot: empty polls are normal again and
    // clear the guard, so a later real snapshot starts counting fresh.
    background.cancel(1);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
    assert_eq!(
        tool.execute(json!({})).await.unwrap().content,
        "No background tasks running."
    );
    bash.execute(json!({"command": "echo fresh; sleep 30", "background": true}))
        .await
        .unwrap();
    let output = tool.execute(json!({})).await.unwrap().content;
    assert!(
        output.starts_with("1 background task(s) running:"),
        "empty poll must reset the guard: {output}"
    );
    background.cancel(2);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
}

#[tokio::test]
async fn completion_delivery_available_requires_an_open_receiver() {
    let mut background = BackgroundTasks::new(Some(Duration::from_secs(30)), None);
    assert!(!background.completion_delivery_available());

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    assert!(background.completion_delivery_available());

    drop(receiver);
    assert!(!background.completion_delivery_available());
    let work_runs = Arc::new(AtomicU64::new(0));
    let work_runs_on_failure = work_runs.clone();
    let error = background
        .spawn_with_id(
            "must not start".into(),
            None,
            None,
            None,
            new_exit_slot(),
            |_| panic!("closed delivery must not call on_id"),
            move || async move {
                work_runs_on_failure.fetch_add(1, Ordering::Relaxed);
                "done".into()
            },
        )
        .unwrap_err();
    assert_eq!(error, "background task delivery is unavailable");
    assert!(background.running().is_empty());
    assert_eq!(work_runs.load(Ordering::Relaxed), 0);

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let started = background
        .spawn_with_id(
            "starts now".into(),
            None,
            None,
            None,
            new_exit_slot(),
            |_| {},
            || async { "done".into() },
        )
        .unwrap();
    assert_eq!(started, "started background task 1: starts now");
    assert!(matches!(
        receiver.recv().await,
        Some(AgentEvent::BackgroundCompleted { id: 1, .. })
    ));
}

#[tokio::test]
async fn panicking_on_id_removes_registration_without_work_or_completion() {
    let mut background = BackgroundTasks::new(Some(Duration::from_secs(30)), None);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let work_runs = Arc::new(AtomicU64::new(0));
    let completions = Arc::new(AtomicU64::new(0));
    let work_runs_in_task = work_runs.clone();
    let completions_in_task = completions.clone();

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = background.spawn_inner(
            "panicking registration".into(),
            None,
            None,
            None,
            None, // owner_session：测试不关心发起者
            new_exit_slot(),
            |_| panic!("controlled on_id panic"),
            move || async move {
                work_runs_in_task.fetch_add(1, Ordering::SeqCst);
                "unexpected".into()
            },
            move |_, _, _| {
                completions_in_task.fetch_add(1, Ordering::SeqCst);
            },
        );
    }));
    assert!(panic.is_err(), "on_id panic must propagate");

    tokio::time::timeout(Duration::from_secs(5), async {
        while !background.running().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("wrapper must remove registration after handoff closes");
    assert_eq!(work_runs.load(Ordering::SeqCst), 0);
    assert_eq!(completions.load(Ordering::SeqCst), 0);
    assert!(receiver.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_on_id_suppresses_work_and_completion() {
    let mut background = BackgroundTasks::new(Some(Duration::from_secs(30)), None);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let work_runs = Arc::new(AtomicU64::new(0));
    let completions = Arc::new(AtomicU64::new(0));
    let spawn_background = background.clone();
    let spawn_entered = entered.clone();
    let spawn_release = release.clone();
    let spawn_work_runs = work_runs.clone();
    let spawn_completions = completions.clone();

    let spawn = tokio::task::spawn_blocking(move || {
        spawn_background.spawn_inner(
            "blocked registration".into(),
            None,
            None,
            None,
            None, // owner_session：测试不关心发起者
            crate::tools::new_exit_slot(),
            move |_| {
                spawn_entered.wait();
                spawn_release.wait();
            },
            move || async move {
                spawn_work_runs.fetch_add(1, Ordering::SeqCst);
                "unexpected".into()
            },
            move |_, _, _| {
                spawn_completions.fetch_add(1, Ordering::SeqCst);
            },
        )
    });

    entered.wait();
    assert_eq!(
        background.cancel(1).as_deref(),
        Some("blocked registration")
    );
    release.wait();
    assert_eq!(
        spawn.await.unwrap().unwrap(),
        "started background task 1: blocked registration"
    );
    tokio::task::yield_now().await;
    assert!(background.running().is_empty());
    assert_eq!(work_runs.load(Ordering::SeqCst), 0);
    assert_eq!(completions.load(Ordering::SeqCst), 1);
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn spawn_with_id_delivers_label_in_background_completed() {
    // Verify that the label registered via spawn_with_id is delivered
    // as BackgroundCompleted.label — sourced from RunningTask, not parsed.
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
    let bg = bash.background.clone();
    bg.spawn_with_id(
        "my custom label".into(),
        None,
        None,
        None,
        crate::tools::new_exit_slot(),
        |_id| {},
        || async { "output from task".into() },
    )
    .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("timeout waiting for completion")
        .unwrap();
    match event {
        AgentEvent::BackgroundCompleted {
            ref label, output, ..
        } => {
            assert_eq!(
                label.as_deref(),
                Some("my custom label"),
                "label must match the registration label"
            );
            assert_eq!(output, "output from task");
        }
        other => panic!("expected BackgroundCompleted, got {other:?}"),
    }
}

/// Verify that BackgroundTasks::start propagates protect_git by checking
/// the parameter is accepted (structural/API coverage). The actual
/// protect_git effect is verified by the bwrap-based tests below.
#[tokio::test]
async fn background_start_accepts_protect_git_parameter() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));

    // Start a background task with protect_git=false (main-agent style).
    bash.execute(json!({"command": "echo main_bg", "background": true}))
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event,
        AgentEvent::BackgroundCompleted { output, .. } if output.contains("main_bg")
    ));
}

#[tokio::test]
async fn sandbox_does_not_protect_git_when_protect_git_is_false() {
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    // Create a workspace with a fake .git directory.
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".git")).unwrap();
    std::fs::write(temp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    // Main agent: protect_git = false → .git is NOT ro-bind.
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // Writing to .git/HEAD must succeed (main agent orchestrates git).
    let write = tool
        .execute(
            json!({"command": "echo 'ref: refs/heads/feature' > .git/HEAD 2>&1; cat .git/HEAD"}),
        )
        .await
        .unwrap()
        .content;
    assert!(
        write.contains("ref: refs/heads/feature"),
        "main agent must be able to write into .git: {write}"
    );
    // The host .git was actually updated.
    assert_eq!(
        std::fs::read_to_string(temp.path().join(".git/HEAD")).unwrap(),
        "ref: refs/heads/feature\n"
    );
}

#[tokio::test]
async fn sandbox_does_not_protect_git_file_when_protect_git_is_false() {
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    // Simulate a linked-worktree .git pointer file.
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join(".git"),
        "gitdir: /some/other/.git/worktrees/x\n",
    )
    .unwrap();

    // Main agent: protect_git = false → .git pointer is writable.
    let tool = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    // Overwriting the .git pointer must succeed.
    let write = tool
        .execute(json!({"command": "echo 'gitdir: /new/path' > .git 2>&1; cat .git"}))
        .await
        .unwrap()
        .content;
    assert!(
        write.contains("/new/path"),
        "main agent must be able to update .git pointer: {write}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join(".git")).unwrap(),
        "gitdir: /new/path\n"
    );
}

#[tokio::test]
async fn background_bash_inherits_protect_git_from_parent_bash() {
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".git")).unwrap();
    std::fs::write(temp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    // Bash with protect_git = true (subagent style).
    // BackgroundTasks must share the same sandbox so bwrap is used.
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut bash = Bash {
        workspace: Workspace::new(temp.path()).unwrap(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30 * 60)), Some(sandbox.clone())),
        sandbox: Some(sandbox),
        protect_git: true,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    bash.set_event_sender(sender);

    // Start background task: it inherits protect_git=true from bash.
    // Writing to .git/HEAD should fail because the background bash
    // also has .git bound read-only.
    let start = bash
        .execute(json!({"command": "echo corrupted > .git/HEAD 2>&1", "background": true}))
        .await
        .unwrap()
        .content;
    assert!(start.starts_with("started background task"), "{start}");
    // Give it time to run and fail.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The host .git must be untouched.
    assert_eq!(
        std::fs::read_to_string(temp.path().join(".git/HEAD")).unwrap(),
        "ref: refs/heads/main\n",
        "background bash with protect_git=true must not corrupt .git"
    );
}

#[tokio::test]
async fn shared_registry_clone_drop_does_not_kill_another_bash_origin() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let registry = BackgroundTasks::new(Some(Duration::from_secs(30)), None);
    let (origin_tx, mut origin_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut bash = Bash {
        workspace: workspace.clone(),
        timeout: Some(Duration::from_secs(30)),
        background: registry.clone(),
        sender: None,
        sandbox: None,
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    bash.set_event_sender(origin_tx);
    // Dropping an unrelated facade must not tear down the shared registry.
    drop(registry);
    bash.execute(json!({"command": "echo origin", "background": true}))
        .await
        .unwrap();
    assert!(matches!(
        origin_rx.recv().await,
        Some(AgentEvent::BackgroundCompleted { output, .. }) if output.contains("origin")
    ));
}

#[tokio::test]
async fn shared_bash_facades_keep_completions_at_their_origins() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let registry = BackgroundTasks::new(Some(Duration::from_secs(30)), None);
    let (first_tx, mut first_rx) = tokio::sync::mpsc::unbounded_channel();
    let (second_tx, mut second_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut first = Bash {
        workspace: workspace.clone(),
        timeout: Some(Duration::from_secs(30)),
        background: registry.clone(),
        sender: None,
        sandbox: None,
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    let mut second = Bash {
        workspace,
        timeout: Some(Duration::from_secs(30)),
        background: registry,
        sender: None,
        sandbox: None,
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    };
    first.set_event_sender(first_tx);
    second.set_event_sender(second_tx);
    first
        .execute(json!({"command": "echo first", "background": true}))
        .await
        .unwrap();
    second
        .execute(json!({"command": "echo second", "background": true}))
        .await
        .unwrap();
    assert!(
        matches!(first_rx.recv().await, Some(AgentEvent::BackgroundCompleted { output, .. }) if output.contains("first"))
    );
    assert!(
        matches!(second_rx.recv().await, Some(AgentEvent::BackgroundCompleted { output, .. }) if output.contains("second"))
    );
    assert!(first_rx.try_recv().is_err());
    assert!(second_rx.try_recv().is_err());
}

// ── TaskSpool ────────────────────────────────────────────────────────────

#[test]
fn task_spool_windows_are_line_aligned_across_chunk_boundaries() {
    let spool = TaskSpool::new();
    spool.append(b"one\ntwo\n");
    spool.append(b"three\nfour\nfive"); // unterminated final line
    assert_eq!(spool.line_count(), 5);
    assert_eq!(spool.len(), "one\ntwo\nthree\nfour\nfive".len());

    let window = spool.window(0, 100, 1024).unwrap();
    assert_eq!(window.first_line, 0);
    assert_eq!(window.line_count, 5);
    assert_eq!(window.lines, ["one", "two", "three", "four", "five"]);
    assert!(!window.truncated);

    // Mid-spool windows start at the requested line, not a chunk boundary.
    let window = spool.window(2, 2, 1024).unwrap();
    assert_eq!(window.first_line, 2);
    assert_eq!(window.lines, ["three", "four"]);

    // Past the end: None.
    assert!(spool.window(5, 1, 1024).is_none());
}

#[test]
fn task_spool_window_obeys_line_and_byte_budgets() {
    let spool = TaskSpool::new();
    spool.append(b"aaaa\nbbbb\ncccc\n");
    assert_eq!(spool.window(0, 2, 1024).unwrap().lines, ["aaaa", "bbbb"]);
    // Byte budget cuts mid-stream at a line boundary.
    let window = spool.window(0, 100, 9).unwrap(); // "aaaa\n" = 5, "bbbb\n" = 5
    assert_eq!(window.lines, ["aaaa", "bbbb"]); // 9 bytes used; "cccc" does not fit
    // An over-long single line keeps its tail plus a marker.
    let spool = TaskSpool::new();
    spool.append(&vec![b'x'; 10_000]);
    spool.append(b"\nshort\n");
    let window = spool.window(0, 10, 128).unwrap();
    assert_eq!(window.lines.len(), 1);
    let line = &window.lines[0];
    assert!(line.ends_with("[line truncated]"), "{line}");
    assert!(line.starts_with(&"x".repeat(128 - "[line truncated]".len() - 1)));
}

#[test]
fn task_spool_handles_multibyte_utf8_across_chunks() {
    let spool = TaskSpool::new();
    let name = "你";
    let bytes = name.as_bytes();
    // Partial character on the unterminated tail is clamped until complete.
    spool.append(&bytes[..1]);
    assert_eq!(spool.window(0, 10, 1024).unwrap().lines, [""]);
    spool.append(&bytes[1..]);
    assert_eq!(spool.window(0, 10, 1024).unwrap().lines, ["你"]);
    spool.append(b"\nok\n");
    assert_eq!(spool.window(0, 10, 1024).unwrap().lines, ["你", "ok"]);
    // Invalid UTF-8 in a terminated line decodes lossy (panel behavior).
    let spool = TaskSpool::new();
    spool.append(b"\xff\xfe\n");
    assert_eq!(
        spool.window(0, 10, 1024).unwrap().lines,
        ["\u{FFFD}\u{FFFD}"]
    );
}

#[test]
fn task_spool_caps_at_16_mib_and_flags_truncated() {
    let spool = TaskSpool::new();
    let chunk = vec![b'a'; 64 * 1024];
    let mut capped = false;
    for _ in 0..(FULL_SPOOL_LIMIT / chunk.len()) + 2 {
        spool.append(&chunk);
        if spool.truncated() {
            capped = true;
            break;
        }
    }
    assert!(capped, "append past the cap must flag truncated");
    assert!(spool.len() <= FULL_SPOOL_LIMIT);
    assert!(spool.line_count() >= 1);
    let window = spool.window(0, 1, 1024).unwrap();
    assert!(window.truncated);
    // The single giant line still windows (tail + marker).
    assert!(window.lines[0].ends_with("[line truncated]"));
}

#[test]
fn task_spool_checkpoints_jump_large_outputs() {
    // > CHECKPOINT_INTERVAL lines: window() must locate any line via the
    // sparse checkpoints (binary search + at most 256-line scan).
    let spool = TaskSpool::new();
    let mut expected = Vec::new();
    for i in 0..1000 {
        let line = format!("line-{i:04}");
        spool.append(line.as_bytes());
        spool.append(b"\n");
        expected.push(line);
    }
    assert_eq!(spool.line_count(), 1000);
    for (start, count) in [(0, 3), (255, 3), (256, 3), (511, 3), (998, 2)] {
        let window = spool.window(start, count, 64 * 1024).unwrap();
        assert_eq!(window.first_line, start);
        assert_eq!(
            window.lines,
            expected[start..start + count],
            "start={start}"
        );
    }
    assert!(spool.window(1000, 1, 1024).is_none());
}

#[test]
fn task_spool_tail_window_covers_the_last_lines() {
    let spool = TaskSpool::new();
    for i in 0..100 {
        spool.append(format!("line {i}\n").as_bytes());
    }
    // A tail window larger than the remaining lines starts at 0.
    let window = spool.window(0, 1000, 64 * 1024).unwrap();
    assert_eq!(window.first_line, 0);
    assert_eq!(window.lines.len(), 100);
    assert_eq!(window.line_count, 100);
}

fn read_image_tool(temp: &tempfile::TempDir, store: &std::path::Path) -> ReadImage {
    ReadImage::with_store(Workspace::new(temp.path()).unwrap(), store.to_path_buf())
}

#[tokio::test]
async fn read_image_stores_hashes_and_returns_structured_images() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("images");
    std::fs::write(temp.path().join("cat.png"), b"png-bytes").unwrap();
    let tool = read_image_tool(&temp, &store);
    let output = tool.execute(json!({"path": "cat.png"})).await.unwrap();
    assert!(output.content.starts_with("[image read: cat.png] (hash "));
    assert!(output.content.ends_with(", image/png, 9 bytes)"));
    assert_eq!(output.images.len(), 1);
    let image = &output.images[0];
    assert_eq!(image.mime, "image/png");
    // The stored file is content-addressed by hash and readable back.
    assert_eq!(
        std::fs::read(store.join(&image.hash)).unwrap(),
        b"png-bytes"
    );

    // Dedup: reading again (even a different path with same bytes) does not
    // add a second file, and the returned hash is identical.
    std::fs::write(temp.path().join("copy.png"), b"png-bytes").unwrap();
    let second = tool.execute(json!({"path": "copy.png"})).await.unwrap();
    assert_eq!(second.images[0].hash, image.hash);
    assert_eq!(std::fs::read_dir(&store).unwrap().count(), 1);
}

#[tokio::test]
async fn read_image_sniffs_mime_from_extension_whitelist() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("images");
    for (name, mime) in [
        ("a.png", "image/png"),
        ("b.jpg", "image/jpeg"),
        ("c.JPEG", "image/jpeg"),
        ("d.webp", "image/webp"),
        ("e.gif", "image/gif"),
    ] {
        std::fs::write(temp.path().join(name), b"bytes").unwrap();
        let tool = read_image_tool(&temp, &store);
        let output = tool.execute(json!({"path": name})).await.unwrap();
        assert_eq!(output.images[0].mime, mime, "for {name}");
    }
}

#[tokio::test]
async fn read_image_rejects_unlisted_extensions_and_oversize_files() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("images");
    std::fs::write(temp.path().join("doc.txt"), b"text").unwrap();
    let tool = read_image_tool(&temp, &store);
    let error = tool.execute(json!({"path": "doc.txt"})).await.unwrap_err();
    assert!(error.contains("unsupported image type for doc.txt"));
    assert!(!std::fs::exists(&store).unwrap());

    // Oversize: 10 MiB + 1 byte.
    std::fs::write(
        temp.path().join("huge.png"),
        vec![0u8; crate::agent::IMAGE_MAX_BYTES + 1],
    )
    .unwrap();
    let error = tool.execute(json!({"path": "huge.png"})).await.unwrap_err();
    assert!(error.contains("exceeding the 10 MiB limit"));
    assert!(!std::fs::exists(&store).unwrap());
}

#[tokio::test]
async fn read_image_respects_workspace_capability_paths() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("images");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x.png"), b"bytes").unwrap();
    let tool = read_image_tool(&temp, &store);
    let error = tool
        .execute(json!({"path": outside.path().join("x.png").to_str().unwrap()}))
        .await
        .unwrap_err();
    assert!(error.contains("absolute path is not within an authorized external root"));
}

#[tokio::test]
async fn read_file_hints_read_image_for_image_extensions() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("photo.jpg"), b"\xff\xd8\xff").unwrap();
    let error = read(&temp, json!({"path": "photo.jpg"})).await.unwrap_err();
    assert!(error.contains("use `read_image` to attach it"));
    // Non-image extensions still read normally.
    std::fs::write(temp.path().join("notes.txt"), b"plain").unwrap();
    assert_eq!(
        read(&temp, json!({"path": "notes.txt"}))
            .await
            .unwrap()
            .content,
        "plain"
    );
}

#[tokio::test]
async fn background_event_sender_is_shared_across_clones() {
    // Regression: LiveSession and the Delegate tool hold separate clones of
    // the BackgroundTasks registry. Agent::new only calls set_event_sender on
    // the Delegate's clone; the LiveSession's clone must still observe the
    // sender (shared via Arc<Mutex<_>>) so btw subagent spawning does not
    // fail with "background task delivery is unavailable".
    let background = BackgroundTasks::new(None, None);
    let mut a = background.clone();
    let b = background.clone();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    a.set_event_sender(tx);
    assert!(b.completion_delivery_available());
    let started = b.spawn_with_id(
        "label".into(),
        None,
        None,
        None,
        new_exit_slot(),
        |_id| {},
        || async move { String::new() },
    );
    assert!(started.is_ok(), "spawn failed: {:?}", started);
}

#[cfg(unix)]
#[test]
fn sandbox_bash_does_not_apply_git_command_scope_config() {
    use std::ffi::OsStr;

    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let sandbox = crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: Vec::new(),
        readable_paths: Vec::new(),
        readable_mounts: Vec::new(),
        writable_mounts: Vec::new(),
    };
    let (command, _plan) = wrap_bash_command(
        &Shell::detect().unwrap(),
        &workspace,
        "true",
        false,
        &sandbox,
    )
    .unwrap();

    // `get_envs` lists only values explicitly applied to this Command, not
    // arbitrary process environment inherited by its child. None of the
    // Git command-scope keys may be e-agent overrides.
    for name in [
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_CONFIG_KEY_1",
        "GIT_CONFIG_VALUE_1",
    ] {
        assert!(
            !command
                .as_std()
                .get_envs()
                .any(|(configured, _)| configured == OsStr::new(name)),
            "sandbox command must not force {name}"
        );
    }
}

#[test]
fn goal_specs_are_closed_and_generic_specs_stay_open() {
    // Review finding: the Goal tool schemas must reject unknown fields at
    // the SCHEMA level (`additionalProperties: false`); generic tool
    // schemas elsewhere must stay open.
    let get = GetGoal.spec();
    assert_eq!(get.parameters["type"], "object");
    assert_eq!(
        get.parameters["additionalProperties"],
        serde_json::Value::Bool(false),
        "get_goal schema must be closed"
    );
    let update = UpdateGoal.spec();
    assert_eq!(
        update.parameters["additionalProperties"],
        serde_json::Value::Bool(false),
        "update_goal schema must be closed"
    );
    for key in [
        "id",
        "revision",
        "action",
        "progress",
        "blocked_reason",
        "success_criteria",
        "evidence",
    ] {
        assert!(
            update.parameters["properties"].get(key).is_some(),
            "update_goal schema must keep `{key}`"
        );
    }
    // Guard rail: the generic `spec()` helper was NOT closed by this
    // change — file/bash tools (built through it) stay open. `web_search`,
    // `get_background_tasks` and `cancel_background_task` were already
    // closed in the base commit by their own spec builders; untouched.
    let preexisting_closed = [
        "web_search",
        "get_background_tasks",
        "cancel_background_task",
        // read_output is deliberately CLOSED (see its spec): the pager
        // accepts exactly ref/offset/limit and nothing else.
        "read_output",
    ];
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (tools, _) = builtins_with_exa_key(workspace, None, None, false, None, None);
    for tool in &tools {
        let name = tool.spec().name;
        if name == "get_goal" || name == "update_goal" {
            continue;
        }
        if preexisting_closed.contains(&name.as_str()) {
            continue;
        }
        let parameters = tool.spec().parameters;
        assert_ne!(
            parameters.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false)),
            "generic tool `{name}` schema must stay open"
        );
    }
}

#[tokio::test]
async fn cancel_overrides_stale_completed_trace_with_killed() {
    // Race contract (oracle finding 3): run_bash may already have written
    // completed/failed into the exit slot before its wrapper removes the
    // task from the registry. Once cancel() successfully removes the task,
    // the delivered trace MUST be killed/SIGKILL, never the stale status.
    let mut background = BackgroundTasks::new(None, None);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    background.set_event_sender(sender.clone());

    let exit_slot = new_exit_slot();
    let slot_for_work = exit_slot.clone();
    // The work writes a stale "completed" trace (as run_bash does on the
    // success path), signals that it did so, then blocks so the task stays
    // registered — exactly the race window before the wrapper removes it.
    let (wrote_tx, wrote_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let started = background
        .spawn_inner(
            "race probe".into(),
            None,
            None,
            None,
            None,
            exit_slot,
            |_| {},
            move || async move {
                *slot_for_work.lock().unwrap() = TaskExit {
                    exit_code: Some(0),
                    signal: None,
                    status: Some("completed".into()),
                };
                let _ = wrote_tx.send(());
                let _ = release_rx.await;
                "done".into()
            },
            move |id, output, trace| {
                let _ = sender.send(AgentEvent::BackgroundCompleted {
                    id,
                    output,
                    label: Some("race probe".into()),
                    started_at_ms: trace.started_at_ms,
                    duration_ms: trace.duration_ms,
                    exit_code: trace.exit_code,
                    signal: trace.signal,
                    status: trace.status,
                    kind: trace.kind,
                });
            },
        )
        .expect("spawn_inner succeeds");
    let id = background.running()[0].id;
    assert!(started.contains(&id.to_string()));

    // Wait until the stale trace is in the slot, then cancel: the cancel
    // path removes the task and must override the stale completed trace.
    let _ = tokio::time::timeout(Duration::from_secs(5), wrote_rx)
        .await
        .expect("work must write the stale slot");
    let label = background.cancel(id).expect("cancel must remove the task");
    assert!(label.contains("race probe"));
    let _ = release_tx.send(());

    match tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("cancel must deliver a completion")
        .unwrap()
    {
        AgentEvent::BackgroundCompleted {
            exit_code,
            signal,
            status,
            kind,
            ..
        } => {
            assert_eq!(exit_code, None, "canceled trace has no exit code");
            assert_eq!(signal.as_deref(), Some("SIGKILL"));
            assert_eq!(status.as_deref(), Some("killed"));
            assert_eq!(kind.as_deref(), Some("delegate"));
        }
        other => panic!("expected BackgroundCompleted, got {other:?}"),
    }
}

#[test]
fn mark_failed_if_empty_fills_only_empty_slots() {
    // Oracle finding 4: run_bash spawn/IO failures return before the exit
    // slot is populated. The fallback fills a truthful "failed" status but
    // never clobbers explicit killed/completed metadata.
    let mut empty = TaskExit::default();
    mark_failed_if_empty(&mut empty);
    assert_eq!(empty.exit_code, None);
    assert_eq!(empty.signal, None);
    assert_eq!(empty.status.as_deref(), Some("failed"));

    let mut killed = TaskExit {
        exit_code: None,
        signal: Some("SIGKILL".into()),
        status: Some("killed".into()),
    };
    mark_failed_if_empty(&mut killed);
    assert_eq!(
        killed.signal.as_deref(),
        Some("SIGKILL"),
        "killed preserved"
    );
    assert_eq!(killed.status.as_deref(), Some("killed"), "killed preserved");

    let mut completed = TaskExit {
        exit_code: Some(0),
        signal: None,
        status: Some("completed".into()),
    };
    mark_failed_if_empty(&mut completed);
    assert_eq!(completed.exit_code, Some(0), "completed preserved");
    assert_eq!(
        completed.status.as_deref(),
        Some("completed"),
        "completed preserved"
    );
}

// --- Ancestor-boundary protection (sandbox mount escape regression) ---
//
// Incident: bwrap mounts /home as a WRITABLE tmpfs and only the exact
// workspace as a writable bind, so the workspace parent stayed a writable
// tmpfs and sandboxed bash could create siblings (`parent/OUTSIDE`,
// `parent/bin/...`). The fix installs the nearest existing host ancestor
// of the workspace as an explicit read-only bind before the workspace
// bind (see `ancestor_guards` in tools/bash.rs).

#[cfg(unix)]
fn ancestor_bash(workspace_dir: &std::path::Path, sandbox: crate::config::Sandbox) -> Bash {
    Bash {
        workspace: Workspace::new(workspace_dir).unwrap(),
        timeout: Some(Duration::from_secs(20)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only: false,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_workspace_parent_is_protected_foreground() {
    // Real bwrap path, temp host layout `parent/workspace`: writes inside
    // the workspace succeed, writes to the parent/siblings fail and never
    // touch the host.
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let parent = tempfile::tempdir().unwrap();
    let workspace_dir = parent.path().join("workspace");
    std::fs::create_dir(&workspace_dir).unwrap();
    std::fs::create_dir(parent.path().join("bin")).unwrap();
    let tool = ancestor_bash(&workspace_dir, sandbox);
    // Inside the workspace: writable as configured.
    tool.execute(json!({"command": "echo hi > inside.txt"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace_dir.join("inside.txt")).unwrap(),
        "hi\n"
    );
    // The workspace parent is the read-only ancestor guard: creating
    // siblings of the workspace must fail.
    let outside = parent.path().join("OUTSIDE");
    let result = tool
        .execute(json!({"command": format!("touch {}/OUTSIDE", parent.path().display())}))
        .await;
    assert!(result.is_err(), "write to workspace parent must fail");
    assert!(!outside.exists(), "host must never gain the escape file");
    // Existing sibling directories are visible but read-only too.
    let result = tool
        .execute(json!({"command": format!("touch {}/bin/EVIL", parent.path().display())}))
        .await;
    assert!(result.is_err(), "write to a sibling directory must fail");
    assert!(!parent.path().join("bin/EVIL").exists());
    // Existing sibling directories are now HIDDEN entirely: the guard
    // shadows the host parent with an empty tmpfs, because importing the
    // host directory would also import pre-existing writable nested host
    // submounts (`--remount-ro` locks only the named mount point, never
    // nested mounts). The path exists (the tmpfs mount point) but the
    // host sibling must not be reachable.
    let listing = tool
        .execute(json!({"command": format!("ls {}", parent.path().display())}))
        .await
        .unwrap()
        .content;
    assert!(listing.contains("workspace"), "{listing}");
    assert!(
        !listing.contains("bin"),
        "unconfigured host siblings must be hidden, not imported: {listing}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_workspace_parent_is_protected_background_and_detached() {
    // The non-detached background and detached paths run through the same
    // `run_bash` / `build_bwrap_plan` as the foreground; verify the
    // boundary holds through both real spawn paths, not only argv shape.
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let parent = tempfile::tempdir().unwrap();
    let workspace_dir = parent.path().join("workspace");
    std::fs::create_dir(&workspace_dir).unwrap();

    // Non-detached background: completion carries the command output.
    let (bash, mut receiver) = background_bash(&parent, Duration::from_secs(30));
    let mut bash = bash;
    bash.workspace = Workspace::new(&workspace_dir).unwrap();
    bash.sandbox = Some(sandbox.clone());
    let started = bash
        .execute(json!({
            "command": format!("touch {}/OUTSIDE_BG && echo ESCAPED || echo BLOCKED", parent.path().display()),
            "background": true
        }))
        .await
        .unwrap()
        .content;
    assert!(started.starts_with("started background task"), "{started}");
    let event = tokio::time::timeout(Duration::from_secs(20), receiver.recv())
        .await
        .expect("timed out waiting for the background completion")
        .unwrap();
    let AgentEvent::BackgroundCompleted { output, .. } = event else {
        panic!("expected BackgroundCompleted");
    };
    assert!(output.contains("BLOCKED"), "{output}");
    assert!(!parent.path().join("OUTSIDE_BG").exists(), "{output}");

    // Detached: no completion is delivered, so the command writes a
    // sentinel INSIDE the workspace (the only writable place) to prove it
    // ran, while the parent write must still be blocked on the host.
    let (bash, _rx) = background_bash(&parent, Duration::from_secs(30));
    let mut bash = bash;
    bash.workspace = Workspace::new(&workspace_dir).unwrap();
    bash.sandbox = Some(sandbox);
    let started = bash
        .execute(json!({
            "command": format!(
                "touch {}/OUTSIDE_DETACHED; touch detached-ran",
                parent.path().display()
            ),
            "background": true,
            "detached": true
        }))
        .await
        .unwrap()
        .content;
    assert!(started.starts_with("started background task"), "{started}");
    // No completion event for detached tasks: poll the in-workspace
    // sentinel (host-visible, workspace is writable through the bind).
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !workspace_dir.join("detached-ran").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "detached sandboxed command never ran"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !parent.path().join("OUTSIDE_DETACHED").exists(),
        "detached bash escaped the ancestor boundary"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_read_only_workspace_parent_guard_keeps_workspace_read_only() {
    // With workspace_writable = false the workspace bind stays read-only
    // AND the ancestor guard must not accidentally re-open anything.
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    sandbox.workspace_writable = false;
    let parent = tempfile::tempdir().unwrap();
    let workspace_dir = parent.path().join("workspace");
    std::fs::create_dir(&workspace_dir).unwrap();
    let tool = ancestor_bash(&workspace_dir, sandbox);
    assert!(
        tool.execute(json!({"command": "touch denied"}))
            .await
            .is_err(),
        "read-only workspace must reject writes"
    );
    assert!(
        tool.execute(json!({"command": format!("touch {}/OUTSIDE", parent.path().display())}))
            .await
            .is_err(),
        "parent must stay protected"
    );
    assert!(!workspace_dir.join("denied").exists());
    assert!(!parent.path().join("OUTSIDE").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_explicit_writable_ancestor_keeps_its_grant() {
    // INTENTIONAL CAPABILITY GRANT: an explicitly configured writable
    // ancestor of the workspace keeps its writable authority — the fix
    // must not silently narrow it. The guard then protects the grant's
    // OWN parent, so unconfigured higher siblings stay read-only.
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let grandparent = tempfile::tempdir().unwrap();
    let parent = grandparent.path().join("parent");
    let workspace_dir = parent.join("workspace");
    std::fs::create_dir(&parent).unwrap();
    std::fs::create_dir(&workspace_dir).unwrap();
    sandbox.writable_paths = vec![parent.to_str().unwrap().to_owned()];
    let tool = ancestor_bash(&workspace_dir, sandbox);
    // The configured writable ancestor retains its grant: siblings of the
    // workspace under it are writable — this is the configured behavior.
    tool.execute(json!({"command": format!("touch {}/GRANTED", parent.display())}))
        .await
        .unwrap();
    assert!(parent.join("GRANTED").exists());
    // The grandparent above the grant is the guard: read-only.
    assert!(
        tool.execute(json!({"command": format!("touch {}/OUTSIDE", grandparent.path().display())}))
            .await
            .is_err(),
        "unconfigured ancestor above the grant must be protected"
    );
    assert!(!grandparent.path().join("OUTSIDE").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_home_tmpfs_shape_parent_guard_regression() {
    // Reproduce the incident shape with temp dirs: the workspace lives two
    // levels below a tmpfs-mounted ancestor (like /home), and only the
    // exact child was bound writable. The parent must now be read-only.
    let Some(sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let home_like = tempfile::tempdir().unwrap();
    let user_like = home_like.path().join("user");
    let parent = user_like.join("openclaw_sandbox");
    let workspace_dir = parent.join("closure");
    std::fs::create_dir(&user_like).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::create_dir(&workspace_dir).unwrap();
    let tool = ancestor_bash(&workspace_dir, sandbox);
    tool.execute(json!({"command": "touch inside.txt"}))
        .await
        .unwrap();
    assert!(workspace_dir.join("inside.txt").exists());
    assert!(
        tool.execute(json!({"command": format!("touch {}/WRITE_TEST", parent.display())}))
            .await
            .is_err(),
        "the incident write must now fail"
    );
    assert!(!parent.join("WRITE_TEST").exists());
    assert!(
        tool.execute(json!({"command": format!("mkdir -p {}/bin && touch {}/bin/x", parent.display(), parent.display())}))
            .await
            .is_err(),
        "the incident bin/ write must now fail"
    );
    assert!(!parent.join("bin").exists());
}

/// True when this process can create mounts in a private user+mount
/// namespace (the prerequisite for staging a nested host submount).
#[cfg(unix)]
fn can_unshare_mounts() -> bool {
    std::process::Command::new("unshare")
        .args(["-rm", "true"])
        .status()
        .ok()
        .is_some_and(|status| status.success())
}

/// Run a REAL bwrap invocation (the production `build_bwrap_plan`
/// argument vector plus the shell program, exactly as `run_bash` spawns
/// it) as a synchronous child process in the CURRENT process's mount
/// namespace. Used by the nested-submount test, which runs inside a
/// staged `unshare -rm` namespace: the bwrap child inherits that staged
/// mount namespace, so the nested tmpfs is genuinely present before
/// bwrap builds its own mounts. Any spawn or non-zero exit is an Err —
/// never silently converted into the expected `blocked` outcome.
#[cfg(unix)]
fn run_real_bwrap_in_current_namespace(
    workspace_dir: &std::path::Path,
    sandbox: &crate::config::Sandbox,
    shell_script: &str,
) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    let workspace = Workspace::new(workspace_dir).map_err(|e| format!("workspace: {e}"))?;
    let root_str = workspace.root().to_string_lossy().into_owned();
    let plan = super::bash::build_bwrap_plan(
        &workspace,
        sandbox,
        false,
        sandbox.network,
        &root_str,
        None,
    )?;
    let shell = Shell::detect().map_err(|e| format!("shell detect: {e}"))?;
    let mut cmd = std::process::Command::new("bwrap");
    cmd.args(&plan.args);
    cmd.arg(&shell.executable);
    cmd.args(shell.command_args(shell_script));
    let numbers = plan.numbers.clone();
    if !numbers.is_empty() {
        // SAFETY: pre_exec runs in the forked child before exec; the
        // closure only touches the plan fds with async-signal-safe fcntl.
        unsafe { cmd.pre_exec(super::bash::clear_cloexec_pre_exec(numbers)) };
    }
    let output = cmd.output().map_err(|e| format!("bwrap spawn: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        return Err(format!(
            "bwrap exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(stdout)
}

/// Body of the nested-submount test, executed INSIDE the staged
/// `unshare -rm` user+mount namespace via `--test-threads=1` recursion.
/// The nested tmpfs is mounted by the caller before this runs; every
/// setup/verification/bwrap failure here is a hard panic (a real test
/// failure), never a masked `blocked`.
#[cfg(unix)]
fn nested_host_submount_test_body() {
    let sandbox = crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: Vec::new(),
        readable_paths: Vec::new(),
        readable_mounts: Vec::new(),
        writable_mounts: Vec::new(),
    };
    let parent = tempfile::tempdir().unwrap();
    let workspace_dir = parent.path().join("workspace");
    let nested = parent.path().join("nested");
    std::fs::create_dir(&workspace_dir).unwrap();
    std::fs::create_dir(&nested).unwrap();
    let parent_str = parent.path().to_string_lossy().into_owned();
    let nested_str = nested.to_string_lossy().into_owned();

    // Stage the nested tmpfs as a REAL pre-existing writable submount
    // under the guarded ancestor, in THIS process's private mount
    // namespace. Any mount failure is a fatal setup error.
    let status = std::process::Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs", &nested_str])
        .status()
        .expect("failed to spawn mount");
    assert!(
        status.success(),
        "staging the nested tmpfs failed: {status}"
    );

    // Sanity, from THIS namespace: the nested tmpfs must be a writable
    // mount point BEFORE bwrap runs. Remove the sentinel afterwards so
    // only sandbox-produced artifacts count below.
    let sentinel = nested.join("STAGING_SENTINEL");
    std::fs::write(&sentinel, b"staged\n").expect("the staged tmpfs must be writable");
    std::fs::remove_file(&sentinel).unwrap();
    let mounted = std::fs::read_to_string("/proc/self/mountinfo")
        .expect("mountinfo must be readable inside the staged namespace");
    assert!(
        mounted.contains(&format!(" {nested_str} ")),
        "the nested tmpfs must be a mount point in the staged namespace"
    );

    // Run the REAL sandboxed bwrap command from within this staged mount
    // namespace: the nested submount is genuinely present before bwrap
    // builds its own mounts. In-sandbox assertions are hard shell
    // failures (no `|| echo blocked` masking): the sandboxed bash must
    // see the guard as an EMPTY tmpfs (the nested host submount and its
    // pre-staged content hidden) and must fail to write through either
    // the nested path or the guard itself.
    let script = format!(
        "set -eu; \
         command -v mountpoint >/dev/null || {{ echo MOUNTPOINT-UNAVAILABLE; exit 1; }}; \
         mountpoint -q / || {{ echo MOUNTPOINT-SELFTEST-FAILED; exit 1; }}; \
         if mp_err=$(mountpoint -q {nested_str} 2>&1); then \
             echo NESTED-STILL-A-MOUNT; exit 1; \
         elif [ $? -ne 1 ]; then \
             echo \"MOUNTPOINT-ERROR: $mp_err\"; exit 1; \
         fi; \
         if [ -e {nested_str} ]; then echo NESTED-VISIBLE; exit 1; fi; \
         if touch {nested_str}/ESCAPED 2>/dev/null; then echo NESTED-WRITABLE; exit 1; fi; \
         if touch {parent_str}/NESTED_WRITE 2>/dev/null; then echo GUARD-WRITABLE; exit 1; fi; \
         echo blocked"
    );
    let out = run_real_bwrap_in_current_namespace(&workspace_dir, &sandbox, &script)
        .unwrap_or_else(|e| panic!("sandboxed bwrap run failed inside the staged namespace: {e}"));
    assert_eq!(
        out.trim(),
        "blocked",
        "sandboxed bash must see neither the nested submount nor the guard as writable: {out}"
    );

    // While the staged namespace is still alive, verify from THIS
    // namespace that no artifact landed through the namespace-local
    // tmpfs, then unmount it and verify the HOST underlying directory is
    // untouched too.
    assert!(
        !nested.join("ESCAPED").exists(),
        "write through a nested host submount under the guard must never land"
    );
    let status = std::process::Command::new("umount")
        .arg(&nested_str)
        .status()
        .expect("failed to spawn umount");
    assert!(
        status.success(),
        "umount of the nested tmpfs failed: {status}"
    );
    assert!(
        !nested.join("ESCAPED").exists(),
        "the host directory beneath the nested submount must be untouched"
    );
    assert!(
        !parent.path().join("NESTED_WRITE").exists(),
        "the guard itself must stay read-only on the host"
    );
    eprintln!(
        "NESTED-EVIDENCE: staged tmpfs at {nested_str} was verified writable and present in \
         /proc/self/mountinfo of the staged namespace; the real bwrap command ran inside that \
         same namespace, saw no nested mount point (`mountpoint -q` inside bwrap), wrote nothing \
         through it, and the guard stayed read-only (probe output: {})",
        out.trim()
    );
}

#[cfg(unix)]
#[test]
fn sandbox_nested_host_submount_under_guard_cannot_escape() {
    // A pre-existing WRITABLE nested host submount beneath the guarded
    // ancestor must not leak into the sandbox: a host bind of the guard
    // would import it wholesale and `--remount-ro` locks only the named
    // mount point, never nested mounts — so the guard must be an empty
    // tmpfs that hides it.
    //
    // Two-phase test so the REAL bwrap invocation inherits the staged
    // mount namespace (nsenter-inside-bwrap cannot see the staging
    // namespace's /proc once bwrap unshares its own PID namespace — that
    // was the false positive this replaces):
    //   phase 1 (this process): `unshare -rm` re-executes the test
    //     binary running ONLY this test with `--test-threads=1`;
    //   phase 2 (inside the namespace): the same test function mounts a
    //     real writable tmpfs at parent/nested, verifies it via
    //     /proc/self/mountinfo, runs the real bwrap sandbox with the
    //     submount already present, and asserts no artifact lands.
    if std::env::var_os("E_AGENT_NESTED_SUBMOUNT_PHASE2").is_some() {
        nested_host_submount_test_body();
        return;
    }
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping nested-submount sandbox test");
        return;
    }
    if !can_unshare_mounts() {
        eprintln!(
            "user+mount namespaces unavailable (unshare -rm failed); skipping nested-submount sandbox test"
        );
        return;
    }
    if !can_mount_tmpfs_in_userns() {
        eprintln!(
            "tmpfs mounts in a user namespace unavailable; skipping nested-submount sandbox test"
        );
        return;
    }
    // Phase 1: re-run this exact test inside a private user+mount
    // namespace. Failure to spawn or a non-zero exit is a FATAL test
    // failure — prerequisites were verified above.
    let exe = std::env::current_exe().expect("cannot locate the test binary");
    let output = std::process::Command::new("unshare")
        .args(["-rm", "--", &exe.to_string_lossy()])
        .args([
            "--exact",
            "tools::tests::sandbox_nested_host_submount_under_guard_cannot_escape",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("E_AGENT_NESTED_SUBMOUNT_PHASE2", "1")
        .output()
        .expect("failed to spawn the namespaced test child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("namespaced nested-submount child output:\n{stdout}\n{stderr}");
    assert!(
        output.status.success(),
        "namespaced nested-submount test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "the namespaced test must have actually run: {stdout}"
    );
    assert!(
        !stdout.contains("0 passed"),
        "the namespaced test was filtered out: {stdout}"
    );
}

/// True when a private user+mount namespace can mount a tmpfs (kernel
/// may allow `unshare -rm` but forbid tmpfs mounts, e.g. some container
/// seccomp/AppArmor profiles).
#[cfg(unix)]
fn can_mount_tmpfs_in_userns() -> bool {
    // Local setup failures (tempdir/create_dir) are FATAL test errors,
    // not a masked environment skip; only the external capability check
    // (unshare/mount/tmpfs) may legitimately report "unavailable".
    let probe = tempfile::tempdir().expect("probe tempdir must be creatable");
    let target = probe.path().join("mnt");
    std::fs::create_dir(&target).expect("probe mount target dir must be creatable");
    let output = std::process::Command::new("unshare")
        .args([
            "-rm",
            "mount",
            "-t",
            "tmpfs",
            "tmpfs",
            &target.to_string_lossy(),
        ])
        .output()
        .expect("failed to spawn the unshare/mount tmpfs probe");
    let ok = output.status.success();
    if !ok {
        eprintln!(
            "tmpfs-in-userns probe failed (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    ok
}

#[cfg(unix)]
#[test]
fn ancestor_guard_plan_ordering_and_edge_cases() {
    // Pure plan tests (no bwrap needed): the guard is an --ro-bind at the
    // workspace parent installed BEFORE the workspace bind, and no later
    // generic mount re-opens the parent.
    // Pure plan construction needs no bwrap installation: build the
    // policy struct directly so these tests also run where bwrap is
    // missing.
    let base = crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: Vec::new(),
        readable_paths: Vec::new(),
        readable_mounts: Vec::new(),
        writable_mounts: Vec::new(),
    };
    let parent = tempfile::tempdir().unwrap();
    let workspace_dir = parent.path().join("workspace");
    std::fs::create_dir(&workspace_dir).unwrap();
    // The policy-parent projection only runs when the policy parent
    // exists on the host; create it (with a config) so the plan really
    // contains the descriptor-pinned projection.
    std::fs::create_dir(workspace_dir.join(".e-agent")).unwrap();
    std::fs::write(workspace_dir.join(".e-agent/config.toml"), b"").unwrap();
    let workspace = Workspace::new(&workspace_dir).unwrap();
    let root_str = workspace.root().to_string_lossy().into_owned();
    let plan =
        super::bash::build_bwrap_plan(&workspace, &base, false, true, &root_str, None).unwrap();
    let args: Vec<String> = plan
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let parent_str = parent.path().to_string_lossy().into_owned();
    let guard = args
        .windows(2)
        .position(|w| w[0] == "--tmpfs" && w[1] == parent_str)
        .expect("ancestor guard tmpfs missing from the plan");
    let lock = args
        .windows(2)
        .position(|w| w[0] == "--remount-ro" && w[1] == parent_str)
        .expect("ancestor guard --remount-ro lock missing from the plan");
    let workspace_bind = args
        .windows(3)
        .rposition(|w| {
            (w[0] == "--bind" || w[0] == "--ro-bind") && w[1] == root_str && w[2] == root_str
        })
        .expect("workspace bind missing from the plan");
    assert!(
        guard < workspace_bind && workspace_bind < lock,
        "ancestor guard must be shadowed first and locked after the workspace bind: {args:?}"
    );
    // Nothing after the lock re-binds the parent itself.
    assert!(
        !args[lock..]
            .windows(3)
            .any(|w| (w[0] == "--bind" || w[0] == "--ro-bind") && w[2] == parent_str),
        "the workspace parent must never be re-bound after its lock: {args:?}"
    );
    // The descriptor-pinned policy-parent projection must remain present
    // and correctly ordered: its `--tmpfs <root>/.e-agent` shadows the
    // policy parent AFTER the workspace bind, so no bind (workspace,
    // alias or explicit descendant) can shadow the policy file.
    let policy_parent_str = workspace
        .policy_anchor()
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let projection = args
        .windows(2)
        .position(|w| w[0] == "--tmpfs" && w[1] == policy_parent_str)
        .unwrap_or_else(|| panic!("policy-parent projection missing from the plan: {args:?}"));
    let projection_lock = args
        .windows(2)
        .rposition(|w| w[0] == "--remount-ro" && w[1] == policy_parent_str)
        .expect("policy-parent projection lock missing from the plan");
    assert!(
        workspace_bind < projection && projection < projection_lock,
        "policy projection must shadow the policy parent after the workspace bind: {args:?}"
    );

    // Edge case: workspace at / fails closed (no parent to protect) —
    // `ancestor_guards` is the fail-closed decision point.
    let error = super::bash::ancestor_guards(std::path::Path::new("/"), &[])
        .expect_err("workspace at / must fail closed");
    assert!(error.contains("fail closed"), "{error}");

    // Edge case: a direct parent that requires guarding / fails closed too.
    let error = super::bash::ancestor_guards(std::path::Path::new("/anything"), &[])
        .expect_err("a guard at / must fail closed");
    assert!(error.contains("fail closed"), "{error}");

    // Edge case: missing intermediate ancestors are skipped — the guard
    // lands on the nearest EXISTING ancestor.
    let gone = parent.path().join("gone");
    let deeper = gone.join("deeper");
    std::fs::create_dir_all(&deeper).unwrap();
    // The workspace root must exist (Workspace::new canonicalizes it); its
    // PARENT `gone` then disappears before the plan is built.
    let deep_ws = Workspace::new(&deeper).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    let deep_root = deep_ws.root().to_string_lossy().into_owned();
    let plan =
        super::bash::build_bwrap_plan(&deep_ws, &base, false, true, &deep_root, None).unwrap();
    let args: Vec<String> = plan
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // The guard skipped the missing parent and pinned the nearest existing
    // ancestor (the temp root), locked read-only before the workspace bind.
    let root_temp = parent.path().to_string_lossy().into_owned();
    let guard = args
        .windows(2)
        .position(|w| w[0] == "--tmpfs" && w[1] == root_temp)
        .unwrap_or_else(|| panic!("guard must shadow the nearest existing ancestor: {args:?}"));
    let lock = args
        .windows(2)
        .position(|w| w[0] == "--remount-ro" && w[1] == root_temp)
        .expect("guard lock missing");
    let workspace_bind = args
        .windows(3)
        .rposition(|w| {
            (w[0] == "--bind" || w[0] == "--ro-bind") && w[1] == deep_root && w[2] == deep_root
        })
        .expect("workspace bind missing");
    assert!(guard < workspace_bind && workspace_bind < lock, "{args:?}");
}

#[cfg(unix)]
#[test]
fn ancestor_guard_symlinked_ancestor_resolves_to_canonical_target() {
    // Regression: when an ancestor component of the workspace is a
    // symlink, the guard must shadow the REAL (canonical) directory, not
    // the lexical symlink path — otherwise the resolved host directory
    // stays reachable and the guard lands on the wrong mount point.
    // Pure plan construction needs no bwrap installation: build the
    // policy struct directly so these tests also run where bwrap is
    // missing.
    let base = crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: Vec::new(),
        readable_paths: Vec::new(),
        readable_mounts: Vec::new(),
        writable_mounts: Vec::new(),
    };
    // Host layout: tempdir/real/workspace, tempdir/link -> real.
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    let workspace_dir = real.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // Enter the workspace THROUGH the symlink so the guard walk starts
    // from the lexical path.
    let workspace = Workspace::new(link.join("workspace")).unwrap();
    let root_str = workspace.root().to_string_lossy().into_owned();
    let plan =
        super::bash::build_bwrap_plan(&workspace, &base, false, true, &root_str, None).unwrap();
    let args: Vec<String> = plan
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let canonical_parent = std::fs::canonicalize(&real).unwrap();
    let canonical_parent_str = canonical_parent.to_string_lossy().into_owned();
    // `ancestor_guards` must pin the canonical target (audit source ==
    // guard destination: the tmpfs has no host source).
    let guards =
        super::bash::ancestor_guards(workspace.root(), &[]).expect("guard computation failed");
    assert_eq!(
        guards,
        vec![(canonical_parent.clone(), canonical_parent)],
        "symlinked ancestor must resolve to its canonical target: {guards:?}"
    );
    // The plan must shadow the CANONICAL directory, ordered before its
    // lock and the workspace bind.
    let guard = args
        .windows(2)
        .position(|w| w[0] == "--tmpfs" && w[1] == canonical_parent_str)
        .expect("guard must shadow the canonical target of the symlinked ancestor");
    let lock = args
        .windows(2)
        .position(|w| w[0] == "--remount-ro" && w[1] == canonical_parent_str)
        .expect("guard lock missing for the canonical target");
    let workspace_bind = args
        .windows(3)
        .rposition(|w| {
            (w[0] == "--bind" || w[0] == "--ro-bind") && w[1] == root_str && w[2] == root_str
        })
        .expect("workspace bind missing");
    assert!(guard < workspace_bind && workspace_bind < lock, "{args:?}");
    // The lexical symlink path must never be mounted directly.
    let link_parent = link.to_string_lossy().into_owned();
    assert!(
        !args.windows(3).any(|w| w[2] == link_parent)
            && !args
                .windows(2)
                .any(|w| w[0] == "--tmpfs" && w[1] == link_parent),
        "the symlink path must never be a mount destination: {args:?}"
    );
}

#[cfg(unix)]
#[test]
fn ancestor_guard_skips_tmp_scratch_and_policy_paths_untouched() {
    // /tmp scratch workspaces need no guard (private tmpfs / host scratch
    // bind); the policy-parent projection must be unaffected by guards.
    // Pure plan construction needs no bwrap installation: build the
    // policy struct directly so these tests also run where bwrap is
    // missing.
    let base = crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: Vec::new(),
        readable_paths: Vec::new(),
        readable_mounts: Vec::new(),
        writable_mounts: Vec::new(),
    };
    let scratch = std::env::temp_dir().join(format!("e-agent-guard-test-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let workspace = Workspace::new(&scratch).unwrap();
    let root_str = workspace.root().to_string_lossy().into_owned();
    let plan =
        super::bash::build_bwrap_plan(&workspace, &base, false, true, &root_str, None).unwrap();
    let args: Vec<String> = plan
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        !args.windows(3).any(|w| {
            (w[0] == "--ro-bind" || w[0] == "--bind") && (w[1] == "/tmp" || w[2] == "/tmp")
        }) && !args
            .windows(2)
            .any(|w| w[0] == "--remount-ro" && w[1] == "/tmp"),
        "/tmp scratch workspaces must gain no guard bind and no guard lock: {args:?}"
    );
    std::fs::remove_dir_all(&scratch).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn common_construction_owner_attribution_does_not_select_tmp_policy() {
    let Some(policy) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let tools = super::tools_with_background_and_exa_key(
        workspace,
        BackgroundTasks::new(None, Some(policy.clone())),
        None,
        Some(policy),
        false,
        false,
        None,
        Some("owner-only".into()),
        false,
        None,
    );
    let bash = tools
        .into_iter()
        .find(|tool| tool.spec().name == "bash")
        .unwrap();
    let output = bash
        .execute(json!({"command": "touch /tmp/e-agent-owner-attribution-tmp-test"}))
        .await
        .unwrap();
    assert!(output.content.contains("exit code: 0"), "{output:?}");
    assert!(!std::path::Path::new("/tmp/e-agent-owner-attribution-tmp-test").exists());
}

#[cfg(unix)]
#[test]
fn sandbox_tmp_policy_plan_differs_only_for_subagents() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let policy = sandbox().unwrap_or(crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable: true,
        writable_paths: Vec::new(),
        readable_paths: Vec::new(),
        readable_mounts: Vec::new(),
        writable_mounts: Vec::new(),
    });
    let root = workspace.root().to_string_lossy().into_owned();
    let main =
        super::bash::build_bwrap_plan(&workspace, &policy, false, true, &root, None).unwrap();
    let sub = super::bash::build_bwrap_plan_with_tmp_policy(
        &workspace, &policy, true, true, &root, None, true,
    )
    .unwrap();
    let main_args: Vec<String> = main
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into())
        .collect();
    let sub_args: Vec<String> = sub
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into())
        .collect();
    assert!(sub_args.windows(2).any(|pair| pair == ["--tmpfs", "/tmp"]));
    assert!(
        sub_args
            .windows(2)
            .any(|pair| pair == ["--remount-ro", "/tmp"])
    );
    assert!(
        !main_args
            .windows(2)
            .any(|pair| pair == ["--remount-ro", "/tmp"])
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn sandbox_facade_tmp_is_writable_for_main_and_read_only_for_subagent() {
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let policy = sandbox().unwrap();
    let main_path = "/tmp/e-agent-main-tmp-test";
    let sub_path = "/tmp/e-agent-subagent-tmp-test";
    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(sub_path);
    let make = |tmp_read_only| Bash {
        workspace: workspace.clone(),
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), Some(policy.clone())),
        sandbox: Some(policy.clone()),
        protect_git: false,
        shell: Shell::detect().unwrap(),
        owner_session: None,
        tmp_read_only,
    };
    let main = make(false);
    let output = main
        .execute(json!({"command": format!("touch {main_path}")}))
        .await
        .unwrap();
    assert!(output.content.contains("exit code: 0"), "{output:?}");
    assert!(!std::path::Path::new(main_path).exists());
    let sub = make(true);
    let output = sub
        .execute(json!({"command": format!("mkdir {sub_path} 2>&1")}))
        .await
        .unwrap_err();
    let output = ToolOutput::text(output);
    assert!(
        output.content.contains("exit code:") || output.content.contains("Read-only file system"),
        "{output:?}"
    );
    assert!(!std::path::Path::new(sub_path).exists());
}
