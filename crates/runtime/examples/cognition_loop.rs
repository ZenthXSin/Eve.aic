//! 三独立进程和性能入口：seed -> run（无新输入）-> run（零重放）。
#[path = "support/cognition.rs"]
mod support;
use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_kernel::backends::FileStateStore;
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use support::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn request(
    socket: &mut TcpStream,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        let mut buffer = [0; 4096];
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Err("请求提前结束".into());
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > 65536 {
            return Err("请求超出验收上限".into());
        }
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let header = std::str::from_utf8(&bytes[..end])?;
            let length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                })
                .ok_or("缺少请求长度")?;
            break (end + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let mut buffer = [0; 4096];
        let read = socket.read(&mut buffer).await?;
        if read == 0 || bytes.len() + read > 65536 {
            return Err("请求正文无效".into());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(serde_json::from_slice(
        &bytes[header_end..header_end + length],
    )?)
}
async fn serve(
    listener: TcpListener,
    count: Arc<AtomicUsize>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        socket.set_nodelay(true)?;
        let input = request(&mut socket).await?;
        count.fetch_add(1, Ordering::SeqCst);
        let messages = input["messages"].as_array().ok_or("缺少消息")?;
        let has_receipt = messages.iter().any(|message| message["role"] == "tool");
        let message = if has_receipt {
            json!({"role":"assistant","content":"完成"})
        } else {
            json!({"role":"assistant","content":null,"tool_calls":[{
                "id":"echo-proof","type":"function","function":{
                    "name":"echo","arguments":"{\"text\":\"cognition-proof\"}"
                }
            }]})
        };
        let body = json!({"id":"local-proof","object":"chat.completion","model":"loopback",
            "choices":[{"index":0,"finish_reason":if has_receipt {"stop"} else {"tool_calls"},"message":message}]}).to_string();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await?;
        socket.shutdown().await?;
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args_os().skip(1);
    let directory = std::path::PathBuf::from(
        args.next()
            .ok_or("用法：cognition_loop 目录 seed|run|cancel")?,
    );
    let mode = args
        .next()
        .ok_or("缺少运行方式")?
        .into_string()
        .map_err(|_| "运行方式非 UTF-8")?;
    if args.next().is_some() || !matches!(mode.as_str(), "seed" | "run" | "cancel") {
        return Err("运行方式无效".into());
    }
    let started = Instant::now();
    let store = Arc::new(FileStateStore::open(&directory)?);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(serve(listener, requests.clone()));
    let provider = Arc::new(OpenAiProvider::new(
        OpenAiConfig::chat("loopback").with_base_url(&endpoint)?,
        "local-test-key",
    )?);
    let rig = Rig::open(store.clone(), provider, None).await;
    let restored_revision = rig.admin.snapshot()?.revision;
    let restore_us = started.elapsed().as_micros();
    let mut stats = LoopStats::default();
    let mut controller = None;
    if mode == "seed" {
        if restored_revision != 0 {
            return Err("seed 只允许空状态".into());
        }
        rig.seed(vec![goal("demo")]);
    } else {
        if mode == "cancel" && restored_revision == 0 {
            rig.seed(vec![goal("demo")]);
        }
        if !rig.admin.snapshot()?.state.goals.contains_key("demo") {
            return Err("必须先 seed".into());
        }
        if mode == "cancel" {
            rig.probe.block.store(true, Ordering::SeqCst);
        }
        let handle = rig.start_loop(options()).await;
        if mode == "cancel" {
            tokio::time::timeout(Duration::from_secs(5), rig.probe.entered.notified()).await?;
            if !handle.cancel_current()? {
                return Err("取消目标未准入".into());
            }
        }
        rig.wait_terminal("demo").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while handle.stats()?.evaluations < 3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, LoopError>(())
        })
        .await??;
        handle.shutdown().await?;
        stats = handle.stats()?;
        controller = Some(handle);
    }
    let snapshot = rig.admin.snapshot()?;
    let target = &snapshot.state.goals["demo"];
    if mode == "run" && target.status != GoalStatus::Completed {
        return Err("目标未实际验证完成".into());
    }
    if mode == "cancel" && target.status != GoalStatus::Cancelled {
        return Err("目标取消状态无效".into());
    }
    let http_requests = requests.load(Ordering::SeqCst);
    let tools = rig.probe.started.load(Ordering::SeqCst);
    let dropped = rig.probe.dropped.load(Ordering::SeqCst);
    if http_requests as u64 != stats.model_requests
        || tools as u64 != stats.started_tools
        || tools != dropped
    {
        return Err("实际请求/工具计数或析构不符".into());
    }
    if mode == "run" && restored_revision == 1 && (http_requests != 2 || tools != 1) {
        return Err("无输入目标未执行一次工具往返".into());
    }
    if mode == "run" && restored_revision > 1 && (http_requests != 0 || tools != 0) {
        return Err("旧目标被重放".into());
    }
    rig.close().await;
    let old_services_closed = rig.admin.snapshot().is_err()
        && controller
            .as_ref()
            .is_none_or(|handle| handle.wake(WakeReason::StateChanged).is_err());
    drop(controller);
    drop(rig);
    drop(store);
    server.abort();
    let _ = server.await;
    let reopened = FileStateStore::open(&directory)?;
    drop(reopened);
    println!(
        "{}",
        json!({
            "mode":mode,"restored_revision":restored_revision,"revision":snapshot.revision,
            "status":target.status,"model_requests":http_requests,"tool_executions":tools,
            "tool_drops":dropped,"admitted_tool_calls":stats.admitted_tool_calls,
            "completed":stats.completed,"cancelled":stats.cancelled,"idle_ticks":stats.idle_ticks,
            "restore_us":restore_us,"wakeup_to_admission_us":stats.last_wakeup_to_admission_us,
            "cancel_settle_us":stats.last_cancel_settle_us,
            "old_services_closed":old_services_closed,"directory_reopened":true
        })
    );
    Ok(())
}
