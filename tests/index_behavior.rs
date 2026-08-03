use std::fs;

use jinfu_search::{SearchIndex, SearchRequest, handle_rpc};
use serde_json::json;
use tempfile::tempdir;

#[cfg(windows)]
use jinfu_search::{call_pipe, serve_pipe, serve_pipe_controlled};

#[cfg(windows)]
use jinfu_search::ntfs::{parse_usn_buffer, resolve_relative_paths};

#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;

#[cfg(windows)]
use tokio::sync::watch;

#[test]
fn scan_replaces_a_root_snapshot_and_searches_unicode_names() {
    let sandbox = tempdir().unwrap();
    let root = sandbox.path().join("资料");
    fs::create_dir_all(root.join("2026")).unwrap();
    fs::write(root.join("2026").join("季度报告.TXT"), "内容").unwrap();
    fs::write(root.join("README.md"), "ignored by this query").unwrap();

    let index = SearchIndex::open(sandbox.path().join("index.db")).unwrap();
    let initial = index.scan_root(&root).unwrap();
    assert_eq!(initial.indexed_files, 2);

    let results = index.search(&SearchRequest::new("季度报告")).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].path.ends_with("季度报告.TXT"));

    fs::remove_file(root.join("README.md")).unwrap();
    fs::write(root.join("2026").join("发票.pdf"), "pdf").unwrap();
    fs::create_dir_all(root.join("pdf-发票资料")).unwrap();
    fs::write(root.join("pdf-发票资料").join("无关.txt"), "noise").unwrap();
    let refreshed = index.scan_root(&root).unwrap();
    assert_eq!(refreshed.indexed_files, 3);

    let removed = index.search(&SearchRequest::new("readme")).unwrap();
    assert!(removed.is_empty());
    let added = index.search(&SearchRequest::new("发票")).unwrap();
    assert_eq!(added.len(), 3);
    assert!(added[0].path.ends_with("发票.pdf"));

    let by_type_and_related_words = index.search(&SearchRequest::new("pdf 发票")).unwrap();
    assert_eq!(by_type_and_related_words.len(), 3);
    assert!(by_type_and_related_words[0].path.ends_with("发票.pdf"));

    let by_type = index.search(&SearchRequest::new("pdf")).unwrap();
    assert!(by_type[0].path.ends_with("发票.pdf"));
}

#[test]
fn agent_protocol_exposes_status_and_search_without_file_contents() {
    let sandbox = tempdir().unwrap();
    let root = sandbox.path().join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("agent-notes.md"), "private file body").unwrap();

    let index = SearchIndex::open(sandbox.path().join("index.db")).unwrap();
    index.scan_root(&root).unwrap();

    let status = handle_rpc(
        &index,
        json!({"jsonrpc": "2.0", "id": 1, "method": "status.get"}),
    );
    assert_eq!(status["result"]["indexed_files"], 1);

    let roots = handle_rpc(
        &index,
        json!({"jsonrpc": "2.0", "id": 2, "method": "index.list_roots"}),
    );
    assert_eq!(roots["result"]["roots"].as_array().unwrap().len(), 1);
    assert_eq!(
        roots["result"]["roots"][0]["root"],
        root.to_string_lossy().replace('\\', "/")
    );

    let search = handle_rpc(
        &index,
        json!({
            "jsonrpc": "2.0",
            "id": "search-1",
            "method": "search.query",
            "params": {"query": "AGENT", "limit": 5}
        }),
    );
    assert_eq!(search["result"]["items"].as_array().unwrap().len(), 1);
    assert!(search["result"]["items"][0].get("contents").is_none());
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_pipe_serves_agent_requests_without_opening_a_tcp_port() {
    let sandbox = tempdir().unwrap();
    let root = sandbox.path().join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("agent-api.txt"), "not indexed as contents").unwrap();

    let index = SearchIndex::open(sandbox.path().join("index.db")).unwrap();
    index.scan_root(&root).unwrap();
    let pipe_name = format!(
        r"\\.\pipe\jinfu-search-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let (shutdown, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(serve_pipe(index, pipe_name.clone(), shutdown_rx));

    // 每次请求都会占用一个新的管道实例。连续请求可验证服务端不会在客户端
    // 读取响应前主动断开连接并丢掉输出缓冲。
    for request_id in 0..24 {
        let response = call_pipe(
            &pipe_name,
            json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "search.query",
                "params": {"query": "agent-api"}
            }),
        )
        .await
        .unwrap_or_else(|error| panic!("第 {request_id} 次本地 Agent 请求失败：{error}"));
        assert_eq!(response["result"]["items"].as_array().unwrap().len(), 1);
    }

    shutdown.send(true).unwrap();
    server.await.unwrap().unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controlled_named_pipe_allows_an_agent_to_stop_the_local_service() {
    let sandbox = tempdir().unwrap();
    let index = SearchIndex::open(sandbox.path().join("index.db")).unwrap();
    let pipe_name = format!(
        r"\\.\pipe\jinfu-search-stop-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let (shutdown, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(serve_pipe_controlled(
        index,
        pipe_name.clone(),
        shutdown_rx,
        shutdown.clone(),
    ));

    let status = call_pipe(
        &pipe_name,
        json!({"jsonrpc": "2.0", "id": "status-before-stop", "method": "status.get"}),
    )
    .await
    .expect("普通请求后服务应继续监听");
    assert!(status.get("result").is_some());

    let response = call_pipe(
        &pipe_name,
        json!({"jsonrpc": "2.0", "id": "stop", "method": "app.shutdown"}),
    )
    .await
    .unwrap();
    assert_eq!(response["result"]["stopping"], true);
    server.await.unwrap().unwrap();
    assert!(*shutdown.borrow());
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_pipe_keeps_running_when_local_clients_fill_available_instances() {
    let sandbox = tempdir().unwrap();
    let index = SearchIndex::open(sandbox.path().join("index.db")).unwrap();
    let pipe_name = format!(
        r"\\.\pipe\jinfu-search-capacity-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let (shutdown, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(serve_pipe(index, pipe_name.clone(), shutdown_rx));
    let mut blockers = Vec::new();
    for _ in 0..16 {
        blockers.push(open_pipe_client(&pipe_name).await);
    }
    assert!(!server.is_finished(), "连接槽耗尽不能让服务退出");

    // 释放一个已被服务读取的连接，让保留的监听实例能够接住第 16 个客户端。
    drop(blockers.remove(0));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    if server.is_finished() {
        panic!("第 16 个客户端不能触发实例上限退出：{:?}", server.await);
    }
    drop(blockers);

    let response = call_pipe(
        &pipe_name,
        json!({"jsonrpc": "2.0", "id": "capacity", "method": "status.get"}),
    )
    .await
    .unwrap();
    assert!(response.get("result").is_some());

    shutdown.send(true).unwrap();
    server.await.unwrap().unwrap();
}

#[cfg(windows)]
async fn open_pipe_client(pipe_name: &str) -> tokio::net::windows::named_pipe::NamedPipeClient {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(client) => return client,
            Err(error) if std::time::Instant::now() < deadline => {
                let _ = error;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(error) => panic!("无法建立测试用本地管道客户端：{error}"),
        }
    }
}

#[cfg(windows)]
#[test]
fn ntfs_usn_parser_resolves_variable_length_unicode_names() {
    let mut buffer = 99_i64.to_le_bytes().to_vec();
    buffer.extend(usn_v2_record(10, 5, "资料", 0x10));
    buffer.extend(usn_v2_record(11, 10, "季度报告.TXT", 0));

    let records = parse_usn_buffer(&buffer).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].name, "季度报告.TXT");

    let paths = resolve_relative_paths(&records);
    assert_eq!(paths.get(&11).unwrap(), "资料/季度报告.TXT");
}

#[cfg(windows)]
#[test]
fn ntfs_paths_ignore_the_sequence_bits_in_file_references() {
    const SEQUENCE: u64 = 7_u64 << 48;
    let mut buffer = 99_i64.to_le_bytes().to_vec();
    buffer.extend(usn_v2_record(SEQUENCE | 10, SEQUENCE | 5, "资料", 0x10));
    buffer.extend(usn_v2_record(
        SEQUENCE | 11,
        SEQUENCE | 10,
        "金福验收.pdf",
        0,
    ));

    let records = parse_usn_buffer(&buffer).unwrap();
    let paths = resolve_relative_paths(&records);

    assert_eq!(paths.get(&11).unwrap(), "资料/金福验收.pdf");
}

#[cfg(windows)]
fn usn_v2_record(frn: u64, parent: u64, name: &str, attributes: u32) -> Vec<u8> {
    let utf16: Vec<u16> = name.encode_utf16().collect();
    let record_length = 60 + utf16.len() * 2;
    let mut record = vec![0_u8; record_length];
    record[0..4].copy_from_slice(&(record_length as u32).to_le_bytes());
    record[4..6].copy_from_slice(&2_u16.to_le_bytes());
    record[8..16].copy_from_slice(&frn.to_le_bytes());
    record[16..24].copy_from_slice(&parent.to_le_bytes());
    record[52..56].copy_from_slice(&attributes.to_le_bytes());
    record[56..58].copy_from_slice(&((utf16.len() * 2) as u16).to_le_bytes());
    record[58..60].copy_from_slice(&60_u16.to_le_bytes());
    for (index, unit) in utf16.into_iter().enumerate() {
        let start = 60 + index * 2;
        record[start..start + 2].copy_from_slice(&unit.to_le_bytes());
    }
    record
}
