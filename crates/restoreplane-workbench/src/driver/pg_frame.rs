//! Bound backend frames before tokio-postgres allocates/decodes their body.
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(super) const MAX_FRAME: usize = 1024 * 1024;

pub(super) struct BoundedPgStream<S> {
    inner: S,
    header: [u8; 5],
    read: usize,
    emitted: usize,
    body_remaining: usize,
    exceeded: Arc<AtomicBool>,
    transaction_status: Option<Arc<AtomicU8>>,
}

impl<S> BoundedPgStream<S> {
    pub(super) fn new(inner: S, exceeded: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            header: [0; 5],
            read: 0,
            emitted: 0,
            body_remaining: 0,
            exceeded,
            transaction_status: None,
        }
    }
    pub(super) fn with_transaction_status(mut self, status: Arc<AtomicU8>) -> Self {
        self.transaction_status = Some(status);
        self
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for BoundedPgStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.exceeded.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WB_PG_FRAME_LIMIT",
            )));
        }
        if this.emitted == 5 && this.body_remaining == 0 {
            this.read = 0;
            this.emitted = 0;
        }
        while this.read < 5 {
            let mut scratch = [0_u8; 5];
            let mut buffer = ReadBuf::new(&mut scratch[..5 - this.read]);
            match Pin::new(&mut this.inner).poll_read(cx, &mut buffer) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {
                    let count = buffer.filled().len();
                    if count == 0 {
                        return if this.read == 0 {
                            Poll::Ready(Ok(()))
                        } else {
                            Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()))
                        };
                    }
                    this.header[this.read..this.read + count].copy_from_slice(buffer.filled());
                    this.read += count;
                }
            }
            if this.read == 5 {
                let length =
                    u32::from_be_bytes(this.header[1..].try_into().expect("four header bytes"))
                        as usize;
                if !(4..=MAX_FRAME).contains(&length) {
                    this.exceeded.store(true, Ordering::SeqCst);
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "WB_PG_FRAME_LIMIT",
                    )));
                }
                this.body_remaining = length - 4;
            }
        }
        if this.emitted < 5 {
            let count = (5 - this.emitted).min(output.remaining());
            output.put_slice(&this.header[this.emitted..this.emitted + count]);
            this.emitted += count;
            return Poll::Ready(Ok(()));
        }
        let mut scratch = [0_u8; 8192];
        let count = this
            .body_remaining
            .min(output.remaining())
            .min(scratch.len());
        let mut buffer = ReadBuf::new(&mut scratch[..count]);
        match Pin::new(&mut this.inner).poll_read(cx, &mut buffer) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                let count = buffer.filled().len();
                if count == 0 {
                    return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                }
                output.put_slice(buffer.filled());
                if this.header[0] == b'Z' && this.body_remaining == 1 && count == 1 {
                    if let Some(status) = &this.transaction_status {
                        status.store(buffer.filled()[0], Ordering::SeqCst);
                    }
                }
                this.body_remaining -= count;
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for BoundedPgStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<S: tokio_postgres::tls::TlsStream + Unpin> tokio_postgres::tls::TlsStream
    for BoundedPgStream<S>
{
    fn channel_binding(&self) -> tokio_postgres::tls::ChannelBinding {
        self.inner.channel_binding()
    }
}

pub(super) struct BoundedTls {
    pub connector: postgres_native_tls::TlsConnector,
    pub exceeded: Arc<AtomicBool>,
    pub transaction_status: Arc<AtomicU8>,
}

impl<S> tokio_postgres::tls::TlsConnect<S> for BoundedTls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = BoundedPgStream<postgres_native_tls::TlsStream<S>>;
    type Error = native_tls::Error;
    type Future = futures::future::BoxFuture<'static, Result<Self::Stream, Self::Error>>;
    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            self.connector.connect(stream).await.map(|stream| {
                BoundedPgStream::new(stream, self.exceeded)
                    .with_transaction_status(self.transaction_status)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn oversized_header_is_refused_before_a_body_is_read_or_exposed() {
        let (mut sender, receiver) = tokio::io::duplex(16);
        let flag = Arc::new(AtomicBool::new(false));
        let mut stream = BoundedPgStream::new(receiver, flag.clone());
        sender
            .write_all(&[b'D', 0x7f, 0xff, 0xff, 0xff])
            .await
            .unwrap();
        let mut output = [0; 32];
        assert_eq!(
            stream.read(&mut output).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(output, [0; 32]);
    }

    #[tokio::test]
    async fn valid_frames_survive_fragmentation_and_zero_length_bodies() {
        let (mut sender, receiver) = tokio::io::duplex(64);
        let bytes = [b'Z', 0, 0, 0, 5, b'I', b'1', 0, 0, 0, 4];
        sender.write_all(&bytes).await.unwrap();
        sender.shutdown().await.unwrap();
        let mut stream = BoundedPgStream::new(receiver, Arc::new(AtomicBool::new(false)));
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, bytes);
    }
}
