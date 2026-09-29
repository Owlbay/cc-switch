//! 响应 body 的空闲超时（设计文档 §5.4）。
//!
//! 每收到一帧就重置计时；超过 `timeout` 没有新帧时返回错误，hyper 随即中断客户端连接
//! （不写终止 chunk）。只影响连接，不改动已转发的字节。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::time::{Instant, Sleep};

use crate::error::BoxError;

/// 空闲超时错误
#[derive(Debug, thiserror::Error)]
#[error("upstream sent no data for {0:?}")]
pub struct IdleTimeout(pub Duration);

/// 给任意 body 加上空闲超时
pub struct IdleTimeoutBody<B> {
    inner: B,
    timeout: Duration,
    sleep: Pin<Box<Sleep>>,
}

impl<B> IdleTimeoutBody<B> {
    pub fn new(inner: B, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            sleep: Box::pin(tokio::time::sleep(timeout)),
        }
    }
}

impl<B> Body for IdleTimeoutBody<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.sleep.as_mut().reset(Instant::now() + this.timeout);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            // 必须 poll sleep 以注册 waker，否则超时永远不会触发
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Some(Err(IdleTimeout(this.timeout).into()))),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tokio::sync::mpsc;

    /// 由测试控制何时产出帧的 body
    struct ChannelBody(mpsc::Receiver<Bytes>);

    impl Body for ChannelBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            self.0
                .poll_recv(cx)
                .map(|item| item.map(|bytes| Ok(Frame::data(bytes))))
        }
    }

    fn channel_body() -> (mpsc::Sender<Bytes>, ChannelBody) {
        let (tx, rx) = mpsc::channel(4);
        (tx, ChannelBody(rx))
    }

    #[tokio::test(start_paused = true)]
    async fn frames_reset_the_timer_and_silence_times_out() {
        let (tx, body) = channel_body();
        let mut body = IdleTimeoutBody::new(body, Duration::from_secs(1));

        tokio::spawn(async move {
            for part in ["a", "b", "c"] {
                tokio::time::sleep(Duration::from_millis(900)).await;
                tx.send(Bytes::from(part)).await.unwrap();
            }
            // 之后保持连接但不再发送
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(tx);
        });

        let mut received = Vec::new();
        let error = loop {
            match body.frame().await {
                Some(Ok(frame)) => received.push(frame.into_data().unwrap()),
                Some(Err(error)) => break error,
                None => panic!("stream ended without timing out"),
            }
        };
        assert_eq!(received, vec!["a", "b", "c"]);
        assert!(error.is::<IdleTimeout>(), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn finished_body_ends_normally() {
        let (tx, body) = channel_body();
        let body = IdleTimeoutBody::new(body, Duration::from_secs(1));
        tx.send(Bytes::from("done")).await.unwrap();
        drop(tx);
        let collected = body.collect().await.unwrap().to_bytes();
        assert_eq!(collected, "done");
    }
}
