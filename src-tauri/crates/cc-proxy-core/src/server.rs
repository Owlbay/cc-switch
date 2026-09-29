//! 入站 HTTP/1.1 服务（设计文档 §9）。
//!
//! 自建 accept loop：用 `hyper::server::conn::http1::Builder` 以便开启
//! `preserve_header_case`，并直接用 `service_fn` 调用 [`Relay::handle`]，所有路由判断在
//! `interface::identify` 中完成。每个连接一个任务；单个请求的转发不再额外 spawn，
//! 客户端断开时 hyper drop 该请求的 future，上游请求随之取消。

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};

use crate::forward::Relay;

/// 在已绑定的监听器上提供服务，直到 `shutdown` 完成后停止接受新连接。
/// 已建立的连接继续处理完当前请求。
pub async fn serve<F>(listener: TcpListener, relay: Arc<Relay>, shutdown: F)
where
    F: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = &mut shutdown => break,
        };
        match accepted {
            Ok((stream, _peer)) => {
                tokio::spawn(serve_connection(stream, Arc::clone(&relay)));
            }
            Err(error) => tracing::warn!(%error, "接受连接失败"),
        }
    }
}

async fn serve_connection(stream: TcpStream, relay: Arc<Relay>) {
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
    if let Err(error) = builder
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        tracing::debug!(%error, "连接结束");
    }
}
