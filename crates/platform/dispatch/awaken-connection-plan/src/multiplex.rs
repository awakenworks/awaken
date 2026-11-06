// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Connection-plan owns bounded byte streams and one yamux driver. Stream drop releases
// @formal reason: only its capacity and owner drop revokes all streams; no application tool identity,
// @formal reason: permission or durable replay is interpreted here.
//! Independent byte streams over one owned attached channel. Application
//! protocols, tool identities, authorization and replay stay with their owners.

use awaken_agent_channel::AgentChannel;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

type Stream = Box<dyn AgentChannel>;
type Open = (oneshot::Sender<io::Result<Stream>>, OwnedSemaphorePermit);

/// One physical connection, bounded stream queues and one driver lifetime.
/// Dropping the owner revokes every stream; dropping a stream affects only that
/// client's channel. Requests never contain application messages or tool state.
pub struct MultiplexedChannel {
    outgoing: mpsc::Sender<Open>,
    incoming: mpsc::Receiver<io::Result<Stream>>,
    driver: tokio::task::JoinHandle<()>,
    capacity: Arc<Semaphore>,
}

impl MultiplexedChannel {
    pub const MAX_STREAMS: usize = 32;

    pub fn client(channel: impl AgentChannel + 'static) -> Self {
        Self::new(channel, yamux::Mode::Client)
    }

    pub fn server(channel: impl AgentChannel + 'static) -> Self {
        Self::new(channel, yamux::Mode::Server)
    }

    fn new(channel: impl AgentChannel + 'static, mode: yamux::Mode) -> Self {
        let mut config = yamux::Config::default();
        // Local admission bounds live streams. The wire also allows a bounded
        // closing tail so a dropped stream can flush FIN/RST before slot reuse.
        config.set_max_num_streams(Self::MAX_STREAMS * 2);
        config.set_max_connection_receive_window(Some(Self::MAX_STREAMS * 2 * 256 * 1024));
        let mut connection = yamux::Connection::new(channel.compat(), config, mode);
        let (outgoing, mut requests) = mpsc::channel::<Open>(Self::MAX_STREAMS);
        let (accepted, incoming) = mpsc::channel(Self::MAX_STREAMS);
        let capacity = Arc::new(Semaphore::new(Self::MAX_STREAMS));
        let inbound_capacity = capacity.clone();
        let driver = tokio::spawn(async move {
            enum Event {
                Opened,
                Inbound(Option<Result<yamux::Stream, yamux::ConnectionError>>),
            }
            let mut pending: Option<Open> = None;
            loop {
                let can_request = pending.is_none();
                tokio::select! {
                    request = requests.recv(), if can_request => {
                        let Some(request) = request else { break };
                        pending = Some(request);
                    }
                    event = poll_fn(|cx| {
                        // Flush prior stream closures before admitting a new one.
                        let inbound = connection.poll_next_inbound(cx);
                        if inbound.is_ready() {
                            return inbound.map(Event::Inbound);
                        }
                        if pending.as_ref().is_some_and(|(reply, _)| reply.is_closed()) {
                            pending.take();
                            return Poll::Ready(Event::Opened);
                        }
                        if pending.is_some()
                            && let Poll::Ready(result) = connection.poll_new_outbound(cx)
                        {
                            let (reply, permit) = pending.take().expect("pending stream request");
                            let result = result.map(|stream| bounded(stream, permit))
                                .map_err(io::Error::other);
                            let _ = reply.send(result);
                            return Poll::Ready(Event::Opened);
                        }
                        Poll::Pending
                    }) => match event {
                        Event::Opened => {},
                        Event::Inbound(Some(Ok(stream))) => {
                            // Never park the connection driver behind one slow
                            // acceptor: queue overflow resets that stream only.
                            if let Ok(permit) = inbound_capacity.clone().try_acquire_owned() {
                                let _ = accepted.try_send(Ok(bounded(stream, permit)));
                            }
                        }
                        Event::Inbound(Some(Err(error))) => {
                            let _ = accepted.try_send(Err(io::Error::other(error)));
                            break;
                        }
                        Event::Inbound(None) => break,
                    }
                }
            }
        });
        Self {
            outgoing,
            incoming,
            driver,
            capacity,
        }
    }

    pub async fn open(&self) -> io::Result<Stream> {
        let permit = self.capacity.clone().try_acquire_owned().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "attached channel stream limit reached",
            )
        })?;
        let (sender, receiver) = oneshot::channel();
        self.outgoing
            .send((sender, permit))
            .await
            .map_err(|_| closed())?;
        receiver.await.map_err(|_| closed())?
    }

    pub async fn accept(&mut self) -> Option<io::Result<Stream>> {
        self.incoming.recv().await
    }
}

impl Drop for MultiplexedChannel {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "attached channel closed")
}

fn bounded(stream: yamux::Stream, permit: OwnedSemaphorePermit) -> Stream {
    Box::new(BoundedStream {
        stream: stream.compat(),
        _permit: permit,
    })
}

struct BoundedStream {
    stream: tokio_util::compat::Compat<yamux::Stream>,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for BoundedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for BoundedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
