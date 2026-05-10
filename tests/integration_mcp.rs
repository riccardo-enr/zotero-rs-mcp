/* Integration test: boot zotero-mcp against a stub localhost Zotero
connector replying with recorded JSON, then drive the MCP server over
its stdio JSON-RPC channel. Asserts:
  * tools/list returns a stable core subset of expected tool names
  * tools/call recent decodes the recorded items into compact records

The test never touches the live Zotero connector; wiremock binds a random
port and the spawned binary is pointed at it via ZOTERO_API_BASE. Any
ambient user config is suppressed by redirecting XDG_CONFIG_HOME / HOME
to a tempdir. */

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE_RECENT: &str = include_str!("fixtures/recent.json");

/* ---- helpers ------------------------------------------------------- */

async fn send(stdin: &mut ChildStdin, msg: &Value) {
    let mut s = serde_json::to_string(msg).unwrap();
    s.push('\n');
    stdin.write_all(s.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

async fn recv_id(reader: &mut BufReader<ChildStdout>, want_id: u64) -> Value {
    /* Read JSON-RPC frames (newline-delimited) until we find one with the
    expected id. Notifications and unrelated frames are skipped. */
    let deadline = Duration::from_secs(10);
    timeout(deadline, async {
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "server closed stdout before id={want_id}");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(|i| i.as_u64()) == Some(want_id) {
                return v;
            }
        }
    })
    .await
    .expect("timed out waiting for jsonrpc response")
}

/* ---- the test ------------------------------------------------------ */

#[tokio::test(flavor = "multi_thread")]
async fn mcp_tools_list_and_recent_against_recorded_fixture() {
    /* 1. Stub Zotero connector ------------------------------------- */
    let server = MockServer::start().await;

    let recent_items: Value = serde_json::from_str(FIXTURE_RECENT).unwrap();

    Mock::given(method("GET"))
        .and(path("/users/0/items"))
        .and(query_param("sort", "dateAdded"))
        .and(query_param("direction", "desc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&recent_items))
        .mount(&server)
        .await;

    let api_base = format!("{}", server.uri()); // e.g. http://127.0.0.1:PORT
                                                /* The client appends paths like `/users/0/items` to ZOTERO_API_BASE
                                                verbatim, so the env var should NOT include a trailing /api segment
                                                for the test stub -- we just expose the bare wiremock URL and mount
                                                paths that match what ZoteroClient builds. */

    /* 2. Spawn the binary in an isolated env ----------------------- */
    let tmp = tempfile::tempdir().unwrap();

    let bin = env!("CARGO_BIN_EXE_zotero-mcp");
    let mut child = Command::new(bin)
        .env("ZOTERO_API_BASE", &api_base)
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("HOME", tmp.path()) /* belt-and-braces against dirs::config_dir() */
        .env("ZOTERO_STORAGE", tmp.path().join("storage"))
        .env("RUST_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zotero-mcp binary");

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    /* 3. MCP handshake -------------------------------------------- */
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "zotero-mcp-itest", "version": "0.0.1"}
            }
        }),
    )
    .await;

    let init_resp = recv_id(&mut reader, 1).await;
    assert!(
        init_resp.get("result").is_some(),
        "initialize failed: {init_resp}"
    );

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await;

    /* 4. tools/list ----------------------------------------------- */
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;

    let list_resp = recv_id(&mut reader, 2).await;
    let tools = list_resp
        .pointer("/result/tools")
        .and_then(|v| v.as_array())
        .expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();

    for required in ["search", "recent", "get", "children"] {
        assert!(
            names.contains(&required),
            "tools/list missing {required}; got {names:?}"
        );
    }

    /* 5. tools/call recent ---------------------------------------- */
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "recent", "arguments": {"n": 2}}
        }),
    )
    .await;

    let call_resp = recv_id(&mut reader, 3).await;
    assert_eq!(
        call_resp.pointer("/result/isError"),
        Some(&Value::Bool(false)),
        "recent tool errored: {call_resp}"
    );

    let text = call_resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .expect("content[0].text");

    let returned: Value = serde_json::from_str(text).expect("recent returns JSON text");
    let arr = returned.as_array().expect("recent returns an array");
    assert_eq!(arr.len(), 2, "expected 2 recorded items, got {}", arr.len());

    let keys: Vec<&str> = arr
        .iter()
        .filter_map(|i| i.get("key").and_then(|k| k.as_str()))
        .collect();
    assert_eq!(keys, vec!["AAAA1111", "BBBB2222"]);

    let titles: Vec<&str> = arr
        .iter()
        .filter_map(|i| i.get("title").and_then(|t| t.as_str()))
        .collect();
    assert_eq!(
        titles,
        vec![
            "Sampling-Based Model Predictive Control for UAV Autonomy",
            "Robust Path Planning Under Wind Disturbance"
        ]
    );

    /* 6. resources/list ------------------------------------------- */
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}),
    )
    .await;

    let res_list = recv_id(&mut reader, 4).await;
    let resources = res_list
        .pointer("/result/resources")
        .and_then(|v| v.as_array())
        .expect("resources array");
    let uris: Vec<&str> = resources
        .iter()
        .filter_map(|r| r.get("uri").and_then(|u| u.as_str()))
        .collect();
    assert!(
        uris.contains(&"zotero://recent"),
        "resources/list missing zotero://recent; got {uris:?}"
    );

    /* 7. resources/templates/list --------------------------------- */
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 5, "method": "resources/templates/list"}),
    )
    .await;

    let tmpl_list = recv_id(&mut reader, 5).await;
    let templates = tmpl_list
        .pointer("/result/resourceTemplates")
        .and_then(|v| v.as_array())
        .expect("resourceTemplates array");
    let tmpl_uris: Vec<&str> = templates
        .iter()
        .filter_map(|r| r.get("uriTemplate").and_then(|u| u.as_str()))
        .collect();
    assert!(
        tmpl_uris.contains(&"zotero://item/{key}")
            && tmpl_uris.contains(&"zotero://item/{key}/fulltext"),
        "templates list missing item URIs; got {tmpl_uris:?}"
    );

    /* 8. resources/read zotero://recent --------------------------- */
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "resources/read",
            "params": {"uri": "zotero://recent"}
        }),
    )
    .await;

    let read_resp = recv_id(&mut reader, 6).await;
    let contents = read_resp
        .pointer("/result/contents")
        .and_then(|v| v.as_array())
        .expect("contents array");
    assert_eq!(contents.len(), 1);
    let entry = &contents[0];
    assert_eq!(
        entry.get("uri").and_then(|v| v.as_str()),
        Some("zotero://recent")
    );
    let body: Value = serde_json::from_str(
        entry
            .get("text")
            .and_then(|v| v.as_str())
            .expect("text body"),
    )
    .expect("recent body is JSON");
    let arr = body.as_array().expect("recent body is array");
    let keys: Vec<&str> = arr
        .iter()
        .filter_map(|i| i.get("key").and_then(|k| k.as_str()))
        .collect();
    assert_eq!(keys, vec!["AAAA1111", "BBBB2222"]);

    /* 9. Tear down -------------------------------------------------- */
    let _ = stdin.shutdown().await;
    drop(stdin);
    let _ = timeout(Duration::from_secs(3), child.wait()).await;
    let _ = child.kill().await;
}
