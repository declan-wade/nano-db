use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::serve::Listener;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, Sleep},
};

pub struct LimitedListener {
    listener: TcpListener,
    connections: Arc<Semaphore>,
    idle_timeout: Duration,
}

impl LimitedListener {
    pub fn new(listener: TcpListener, max_connections: usize, idle_timeout: Duration) -> Self {
        Self {
            listener,
            connections: Arc::new(Semaphore::new(max_connections)),
            idle_timeout,
        }
    }
}

impl Listener for LimitedListener {
    type Io = LimitedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let permit = match self.connections.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(error) => {
                    tracing::error!(error = %error, "connection admission is closed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            match self.listener.accept().await {
                Ok((stream, address)) => {
                    return (
                        LimitedStream {
                            stream,
                            _permit: permit,
                            timeout: self.idle_timeout,
                            idle: Box::pin(tokio::time::sleep(self.idle_timeout)),
                        },
                        address,
                    );
                }
                Err(error) => {
                    tracing::error!(error = %error, "accepting a TCP connection failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

pub struct LimitedStream {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
    timeout: Duration,
    idle: Pin<Box<Sleep>>,
}

impl LimitedStream {
    fn progress(&mut self) {
        self.idle.as_mut().reset(Instant::now() + self.timeout);
    }

    fn pending<T>(&mut self, context: &mut Context<'_>) -> Poll<io::Result<T>> {
        match self.idle.as_mut().poll(context) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection idle deadline exceeded",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buffer.filled().len();
        match Pin::new(&mut self.stream).poll_read(context, buffer) {
            Poll::Ready(result) => {
                if result.is_ok() && buffer.filled().len() > before {
                    self.progress();
                }
                Poll::Ready(result)
            }
            Poll::Pending => self.pending(context),
        }
    }
}

impl AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write(context, bytes) {
            Poll::Ready(result) => {
                if result.as_ref().is_ok_and(|written| *written > 0) {
                    self.progress();
                }
                Poll::Ready(result)
            }
            Poll::Pending => self.pending(context),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_flush(context) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => self.pending(context),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_shutdown(context) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => self.pending(context),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write_vectored(context, buffers) {
            Poll::Ready(result) => {
                if result.as_ref().is_ok_and(|written| *written > 0) {
                    self.progress();
                }
                Poll::Ready(result)
            }
            Poll::Pending => self.pending(context),
        }
    }
}
