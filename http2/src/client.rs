use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::io;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;

use futures;
use futures::Future;
use futures::Stream;

use tokio_core::reactor;

use solicit::http::HttpResult;
use solicit::http::HttpError;
use solicit::http::HttpScheme;
use solicit::http::StaticHeader;

use solicit_async::*;

use client_conn::*;
use http_common::*;


// Data sent from event loop to GrpcClient
struct LoopToClient {
    // used only once to send shutdown signal
    shutdown_tx: futures::sync::mpsc::UnboundedSender<()>,
    _loop_handle: reactor::Remote,
    http_conn: Arc<HttpClientConnectionAsync>,
}


pub struct Http2Client {
    loop_to_client: LoopToClient,
    thread_join_handle: Option<thread::JoinHandle<()>>,
    host: String,
    http_scheme: HttpScheme,
}

impl Http2Client {
    pub fn new(host: &str, port: u16, tls: bool) -> HttpResult<Http2Client> {

        // TODO: sync
        // TODO: try connect to all addrs
        let socket_addr = (host, port).to_socket_addrs()?.next().unwrap();

        // We need some data back from event loop.
        // This channel is used to exchange that data
        let (get_from_loop_tx, get_from_loop_rx) = mpsc::channel();

        // Start event loop.
        let join_handle = thread::spawn(move || {
            run_client_event_loop(socket_addr, tls, get_from_loop_tx);
        });

        // Get back call channel and shutdown channel.
        let loop_to_client = get_from_loop_rx.recv()
            .map_err(|_| HttpError::IoError(io::Error::new(io::ErrorKind::Other, "get response from loop")))?;

        Ok(Http2Client {
            loop_to_client: loop_to_client,
            thread_join_handle: Some(join_handle),
            host: host.to_owned(),
            http_scheme: if tls { HttpScheme::Https } else { HttpScheme::Http },
        })
    }

    pub fn start_request(
        &self,
        headers: Vec<StaticHeader>,
        body: HttpStreamSend<Vec<u8>>)
            -> HttpStreamStreamSend
    {
        self.loop_to_client.http_conn.start_request(headers, body)
    }

    pub fn dump_state(&self) -> HttpFutureSend<ConnectionState> {
        self.loop_to_client.http_conn.dump_state()
    }
}

// Event loop entry point
fn run_client_event_loop(
    socket_addr: SocketAddr,
    tls: bool,
    send_to_back: mpsc::Sender<LoopToClient>)
{
    // Create an event loop.
    let mut lp = reactor::Core::new().unwrap();

    // Create a channel to receive shutdown signal.
    let (shutdown_tx, shutdown_rx) = futures::sync::mpsc::unbounded();

    let (http_conn, http_conn_future) =
        if tls {
            HttpClientConnectionAsync::new_tls(lp.handle(), &socket_addr)
        } else {
            HttpClientConnectionAsync::new_plain(lp.handle(), &socket_addr)
        };
    let http_conn_future: HttpFuture<_> = Box::new(http_conn_future.map_err(HttpError::from));

    // Send channels back to GrpcClient
    send_to_back
        .send(LoopToClient {
            shutdown_tx: shutdown_tx,
            _loop_handle: lp.remote(),
            http_conn: Arc::new(http_conn),
        })
        .expect("send back");

    let shutdown = shutdown_rx.into_future()
        .map_err(|((), _)| HttpError::IoError(io::Error::new(io::ErrorKind::Other, "shutdown_rx")))
        .and_then(move |_| {
            // Must complete with error,
            // so `join` with this future cancels another future.
            futures::failed::<(), _>(HttpError::IoError(io::Error::new(io::ErrorKind::Other, "shutdown")))
        });

    // Wait for either completion of connection (i. e. error)
    // or shutdown signal.
    let done = http_conn_future.join(shutdown);

    // TODO: do not ignore error
    lp.run(done).ok();
}

// We shutdown the client in the destructor.
impl Drop for Http2Client {
    fn drop(&mut self) {
        // ignore error because even loop may be already dead
        self.loop_to_client.shutdown_tx.send(()).ok();

        // do not ignore errors because we own event loop thread
        self.thread_join_handle.take().expect("handle.take")
            .join().expect("join thread");
    }
}
