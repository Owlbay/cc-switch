//! 入站 HTTP/1.1 服务（设计文档 §9）。
//!
//! 自建 accept loop：用 `hyper::server::conn::http1::Builder` 以便开启
//! `preserve_header_case`，并直接用 `service_fn` 调用 [`Relay::handle`]，所有路由判断在
//! `interface::identify` 中完成。每个连接一个任务；单个请求的转发不再额外 spawn，
//! 客户端断开时 hyper drop 该请求的 future，上游请求随之取消。
//!
//! 优雅退出：`shutdown` 完成后停止接受新连接，并通知每个连接 `graceful_shutdown`——
//! 空闲的 keep-alive 连接立即关闭，进行中的请求（含 SSE 流）继续处理完。所有连接结束后
//! `serve` 才返回；需要限时的调用方自行用超时包住 `serve`。

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::forward::Relay;

/// 在已绑定的监听器上提供服务，直到 `shutdown` 完成且所有连接处理完毕。
pub async fn serve<F>(listener: TcpListener, relay: Arc<Relay>, shutdown: F)
where
    F: Future<Output = ()>,
{
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        // 回收已结束的连接任务，避免长期运行时 JoinSet 无限增长
        while connections.try_join_next().is_some() {}
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = &mut shutdown => break,
        };
        match accepted {
            Ok((stream, _peer)) => {
                connections.spawn(serve_connection(
                    stream,
                    Arc::clone(&relay),
                    stop_rx.clone(),
                ));
            }
            Err(error) => tracing::warn!(%error, "接受连接失败"),
        }
    }

    drop(listener);
    let _ = stop_tx.send(true);
    if !connections.is_empty() {
        tracing::info!(
            connections = connections.len(),
            "停止接受新连接，等待进行中的请求结束"
        );
    }
    while connections.join_next().await.is_some() {}
}

async fn serve_connection(stream: TcpStream, relay: Arc<Relay>, mut stop: watch::Receiver<bool>) {
    let _ = stream.set_nodelay(true);
    let preserve_header_case = relay.preserve_header_case();
    let service = service_fn(move |request| {
        let relay = Arc::clone(&relay);
        async move { Ok::<_, Infallible>(relay.handle(request).await) }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .preserve_header_case(preserve_header_case);
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);

    let result = tokio::select! {
        result = connection.as_mut() => result,
        // 停止信号只会从 false 变为 true 一次；发送端被 drop 同样视为停止
        _ = stop.changed() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    };
    if let Err(error) = result {
        tracing::debug!(%error, "连接结束");
    }
}
