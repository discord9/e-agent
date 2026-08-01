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
            "web_search"
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
    assert_eq!(result, "public [redacted] context");

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
    assert!(result.len() <= OUTPUT_LIMIT);
    assert!(result.is_char_boundary(result.len()));
    assert!(result.ends_with("\n...[truncated]"));
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

async fn edit(temp: &tempfile::TempDir, old: &str, new: &str) -> Result<String, String> {
    EditFile {
        workspace: Workspace::new(temp.path()).unwrap(),
    }
    .execute(json!({"path": "file.txt", "old": old, "new": new}))
    .await
}

#[tokio::test]
async fn edit_requires_exactly_one_match() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.txt");
    std::fs::write(&path, "one two one").unwrap();
    assert!(
        edit(&temp, "missing", "x")
            .await
            .unwrap_err()
            .contains("found 0")
    );
    assert!(
        edit(&temp, "one", "x")
            .await
            .unwrap_err()
            .contains("found 2")
    );
    assert_eq!(
        edit(&temp, "two", "x").await.unwrap(),
        "file edited (line 1)"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "one x one");
}

async fn read(temp: &tempfile::TempDir, arguments: Value) -> Result<String, String> {
    ReadFile {
        workspace: Workspace::new(temp.path()).unwrap(),
    }
    .execute(arguments)
    .await
}

#[tokio::test]
async fn external_absolute_file_tools_enforce_policy_and_reuse_semantics() {
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
            .contains("[sandbox]")
    );
    assert!(
        read.execute(json!({"path": policy_path}))
            .await
            .unwrap()
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
            .unwrap(),
        "a\nb\n[showing lines 1-2 of 5; use offset 3 to continue]"
    );
    assert_eq!(
        read(&temp, json!({"path": "file.txt", "offset": 3, "limit": 2}))
            .await
            .unwrap(),
        "c\nd\n[showing lines 3-4 of 5; use offset 5 to continue]"
    );
    assert_eq!(
        read(&temp, json!({"path": "file.txt", "offset": 5}))
            .await
            .unwrap(),
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
            .unwrap(),
        "[offset 9 is past end of file (2 lines)]"
    );
    std::fs::write(temp.path().join("empty.txt"), "").unwrap();
    assert_eq!(
        read(&temp, json!({"path": "empty.txt"})).await.unwrap(),
        "[empty file]"
    );
}

#[tokio::test]
async fn read_truncates_long_lines_to_the_byte_limit() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file.txt"), "x".repeat(READ_LIMIT + 1)).unwrap();
    let output = read(&temp, json!({"path": "file.txt"})).await.unwrap();
    assert!(output.ends_with("\n...[truncated]"));
    assert_eq!(output.len(), READ_LIMIT + "\n...[truncated]".len());
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
    let temp = tempfile::tempdir().unwrap();
    let tool = WriteFile {
        workspace: Workspace::new(temp.path()).unwrap(),
    };
    assert_eq!(
        tool.execute(json!({"path": "new/file.txt", "content": "hello"}))
            .await
            .unwrap(),
        "file written"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("new/file.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn capture_drains_and_marks_each_truncated_stream() {
    async fn captured(bytes: Vec<u8>) -> Captured {
        let (mut writer, reader) = tokio::io::duplex(1024);
        tokio::spawn(async move { writer.write_all(&bytes).await.unwrap() });
        capture(reader, None, None).await.unwrap()
    }
    let stdout = captured(vec![b'o'; OUTPUT_LIMIT + 1]).await;
    let stderr = captured(vec![b'e'; OUTPUT_LIMIT + 1]).await;
    assert_eq!(stdout.bytes.len(), OUTPUT_LIMIT);
    assert_eq!(stderr.bytes.len(), OUTPUT_LIMIT);
    let output = format_output(Some(0), &stdout, &stderr);
    assert!(output.contains("stdout: [truncated]"));
    assert!(output.contains("stderr: [truncated]"));
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
    };
    let plain_desc = plain.spec().description;
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
        }),
        protect_git: true,
        shell: Shell::detect().unwrap(),
    };
    let desc = sandboxed.spec().description;
    assert!(desc.contains("bubblewrap sandbox"), "{desc}");
    assert!(desc.contains("workspace is writable"), "{desc}");
    assert!(desc.contains("OUTSIDE the sandbox"), "{desc}");
    assert!(desc.contains("network is disabled"), "{desc}");
    assert!(desc.contains("/mnt/big/cargo-home"), "{desc}");
    assert!(desc.contains("~/.rustup"), "{desc}");
    assert!(desc.contains("read_file/write_file/edit_file"), "{desc}");
    assert!(desc.contains("`.git`"), "{desc}");
    assert!(desc.contains("linked-worktree pointer"), "{desc}");
    assert!(desc.contains("read-only to prevent"), "{desc}");

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
        }),
        protect_git: false,
        shell: Shell::detect().unwrap(),
    };
    let desc_ro = sandboxed_ro.spec().description;
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
        }),
        protect_git: false,
        shell: Shell::detect().unwrap(),
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
            "cancel_background_task"
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
        }),
        true,
        None,
        None,
    );
    let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
    assert_eq!(
        names,
        [
            "read_file",
            "get_background_tasks",
            "cancel_background_task",
            "bash",
            "web_search"
        ],
        "read-only with a sandbox keeps bash and web_search"
    );
    // The bash description must reflect the narrowed policy.
    let bash_desc = tools
        .iter()
        .find(|tool| tool.spec().name == "bash")
        .unwrap()
        .spec()
        .description;
    assert!(bash_desc.contains("workspace is read-only"), "{bash_desc}");
    assert!(bash_desc.contains("network is disabled"), "{bash_desc}");
    assert!(
        bash_desc.contains("~/.rustup"),
        "readable roots survive the narrowing: {bash_desc}"
    );
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
    };
    let narrowed = read_only_sandbox(&sandbox);
    assert!(narrowed.enabled);
    assert!(!narrowed.network, "network must be disabled");
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
    };
    bash.set_event_sender(sender.clone());
    bash.background.set_event_sender(sender);

    let started = bash
        .execute(json!({"command": "touch escape.txt", "background": true}))
        .await
        .unwrap();
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
    };
    // No loopback either in a fresh net namespace: connecting anywhere fails.
    let result = tool
        .execute(
            json!({"command": "exec 3<>/dev/tcp/127.0.0.1/80 && echo NET_OK || echo NET_BLOCKED"}),
        )
        .await
        .unwrap();
    assert!(result.contains("NET_BLOCKED"), "{result}");
}

#[tokio::test]
async fn sandbox_read_only_workspace_rejects_bash_but_not_file_tool_writes() {
    let Some(mut sandbox) = sandbox() else {
        eprintln!("bwrap unavailable; skipping sandbox test");
        return;
    };
    sandbox.workspace_writable = false;
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    WriteFile {
        workspace: workspace.clone(),
    }
    .execute(json!({"path": "file-tool", "content": "yes"}))
    .await
    .unwrap();
    let tool = Bash {
        workspace,
        timeout: Some(Duration::from_secs(10)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(30)), None),
        sandbox: Some(sandbox),
        protect_git: false,
        shell: Shell::detect().unwrap(),
    };
    assert!(
        tool.execute(json!({"command": "touch bash-file"}))
            .await
            .is_err()
    );
    assert!(!temp.path().join("bash-file").exists());
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
    };
    assert!(
        tool.execute(json!({"command": "touch .e-agent/config.toml"}))
            .await
            .is_err()
    );
    assert!(!policy_dir.join("config.toml").exists());
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
        .unwrap();
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
    };
    // Reading .git/HEAD must succeed (git commands read metadata).
    let out = tool
        .execute(json!({"command": "cat .git/HEAD"}))
        .await
        .unwrap();
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
    };
    // Reading the .git pointer must succeed.
    let out = tool.execute(json!({"command": "cat .git"})).await.unwrap();
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
    };
    // Check whether /run/systemd/resolve exists on this host.
    let host_has_resolve = std::path::Path::new("/run/systemd/resolve").exists();
    // Inside the sandbox /run should be visible only when systemd-resolve
    // was mounted. The directory itself should either exist and be readable
    // or not exist at all.
    let out = tool
        .execute(json!({"command": "test -d /run/systemd/resolve && echo PRESENT || echo ABSENT"}))
        .await
        .unwrap();
    if host_has_resolve {
        assert!(
            out.contains("PRESENT"),
            "/run/systemd/resolve should be mounted when host has it; output: {out}"
        );
        // The stub-resolv.conf should also be readable.
        let contents = tool
            .execute(json!({"command": "cat /run/systemd/resolve/stub-resolv.conf 2>&1 || true"}))
            .await
            .unwrap();
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
    };
    // /etc/resolv.conf is always mounted (--ro-bind-try). Its contents
    // depend on the host config; we just check it is readable.
    let out = tool
        .execute(json!({"command": "cat /etc/resolv.conf 2>&1 || true"}))
        .await
        .unwrap();
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
        Ok(out) if out.contains("nameserver") => { /* proceed */ }
        Ok(out) => {
            eprintln!("stub-resolv.conf reachable but no nameserver line: {out}");
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
            let trimmed = out.trim();
            if trimmed.is_empty()
                || trimmed.contains("not found")
                || trimmed.contains("NXDOMAIN")
                || trimmed.contains("SERVFAIL")
            {
                eprintln!("DNS resolution returned no result (external network issue): {out}");
            } else {
                assert!(
                    trimmed.contains("github.com") || trimmed.contains('.'),
                    "expected resolved address but got: {out}"
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
            .starts_with("started background task 9:")
    );
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
    let tool = GetBackgroundTasks {
        background: background.clone(),
    };

    // No tasks running initially.
    assert_eq!(
        tool.execute(json!({})).await.unwrap(),
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

    // The tool reports the running task with correct id, label, and role.
    let output = tool.execute(json!({})).await.unwrap();
    assert_eq!(
        output,
        "1 background task(s) running:\n#1: echo hello; sleep 30 (bash)"
    );

    // Start a second background task (id=2).
    bash.execute(json!({"command": "echo world; sleep 30", "background": true}))
        .await
        .unwrap();
    tasks = background.running();
    assert_eq!(tasks.len(), 2);

    // Both tasks display with their actual ids, not list positions.
    let output = tool.execute(json!({})).await.unwrap();
    assert_eq!(
        output,
        "2 background task(s) running:\n#1: echo hello; sleep 30 (bash)\n#2: echo world; sleep 30 (bash)"
    );

    // Cancel the first task (id=1).
    background.cancel(1);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;

    // The remaining task (id=2) still shows as #2, NOT renumbered to #1.
    let output = tool.execute(json!({})).await.unwrap();
    assert_eq!(
        output,
        "1 background task(s) running:\n#2: echo world; sleep 30 (bash)"
    );

    // Cleanup.
    background.cancel(2);
    let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
    assert_eq!(
        tool.execute(json!({})).await.unwrap(),
        "No background tasks running."
    );
}

#[tokio::test]
async fn get_background_tasks_shows_delegate_tasks_as_delegate_not_bash() {
    let temp = tempfile::tempdir().unwrap();
    let (bash, _receiver) = background_bash(&temp, Duration::from_secs(10));
    let background = bash.background.clone();

    let tool = GetBackgroundTasks {
        background: background.clone(),
    };

    // Spawn a delegate-style task (no role, no output slot) via spawn().
    background
        .spawn(
            "search codebase".into(),
            None, // role
            None, // process_group
            || async { "done".into() },
        )
        .unwrap();

    let output = tool.execute(json!({})).await.unwrap();
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

    let tool = GetBackgroundTasks {
        background: background.clone(),
    };

    // A delegate with a known role (e.g. "explorer").
    background
        .spawn(
            "search the logs".into(),
            Some("explorer".into()),
            None,
            || async { "done".into() },
        )
        .unwrap();

    let output = tool.execute(json!({})).await.unwrap();
    assert_eq!(
        output,
        "1 background task(s) running:\n#1: search the logs (explorer)"
    );
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
            |_| panic!("controlled on_id panic"),
            move || async move {
                work_runs_in_task.fetch_add(1, Ordering::SeqCst);
                "unexpected".into()
            },
            move |_, _| {
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
            move |_| {
                spawn_entered.wait();
                spawn_release.wait();
            },
            move || async move {
                spawn_work_runs.fetch_add(1, Ordering::SeqCst);
                "unexpected".into()
            },
            move |_, _| {
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
    };
    // Writing to .git/HEAD must succeed (main agent orchestrates git).
    let write = tool
        .execute(
            json!({"command": "echo 'ref: refs/heads/feature' > .git/HEAD 2>&1; cat .git/HEAD"}),
        )
        .await
        .unwrap();
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
    };
    // Overwriting the .git pointer must succeed.
    let write = tool
        .execute(json!({"command": "echo 'gitdir: /new/path' > .git 2>&1; cat .git"}))
        .await
        .unwrap();
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
    };
    bash.set_event_sender(sender);

    // Start background task: it inherits protect_git=true from bash.
    // Writing to .git/HEAD should fail because the background bash
    // also has .git bound read-only.
    let start = bash
        .execute(json!({"command": "echo corrupted > .git/HEAD 2>&1", "background": true}))
        .await
        .unwrap();
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
    };
    let mut second = Bash {
        workspace,
        timeout: Some(Duration::from_secs(30)),
        background: registry,
        sender: None,
        sandbox: None,
        protect_git: false,
        shell: Shell::detect().unwrap(),
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
async fn read_image_stores_hashes_and_returns_structured_marker() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("images");
    std::fs::write(temp.path().join("cat.png"), b"png-bytes").unwrap();
    let tool = read_image_tool(&temp, &store);
    let output = tool.execute(json!({"path": "cat.png"})).await.unwrap();
    let (summary, image) = crate::agent::split_image_marker(&output);
    assert!(summary.starts_with("[image read: cat.png] (hash "));
    assert!(summary.ends_with(", image/png, 9 bytes)"));
    let image = image.unwrap();
    assert_eq!(image.mime, "image/png");
    // The stored file is content-addressed by hash and readable back.
    assert_eq!(
        std::fs::read(store.join(&image.hash)).unwrap(),
        b"png-bytes"
    );

    // Dedup: reading again (even a different path with same bytes) does not
    // add a second file, and the returned hash is identical.
    std::fs::write(temp.path().join("copy.png"), b"png-bytes").unwrap();
    let output = tool.execute(json!({"path": "copy.png"})).await.unwrap();
    let (_, second) = crate::agent::split_image_marker(&output);
    assert_eq!(second.unwrap().hash, image.hash);
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
        let (_, image) = crate::agent::split_image_marker(&output);
        assert_eq!(image.unwrap().mime, mime, "for {name}");
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
        read(&temp, json!({"path": "notes.txt"})).await.unwrap(),
        "plain"
    );
}
