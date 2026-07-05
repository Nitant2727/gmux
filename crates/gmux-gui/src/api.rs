//! The automation-API bridge: pipe handler threads parse [`gmux_proto`] requests and forward them
//! to the winit main thread (which owns the `Session`) over a channel, waking the event loop via
//! `EventLoopProxy`. The main thread executes the call against the mux state and replies through a
//! per-request back-channel.

use std::io::BufReader;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use gmux_pipe::{PipeServer, PipeStream};
use gmux_proto::{read_msg, write_msg, Request, Response};
use winit::event_loop::EventLoopProxy;

/// How long a pipe client waits for the main thread to service a call.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// A request in flight from a pipe thread to the main thread.
pub struct ApiCommand {
    pub request: Request,
    pub reply: Sender<Response>,
}

/// Start the automation pipe server. Each connection gets a thread that loops
/// read-request → forward to main thread → write-response until the client disconnects.
pub fn start(
    pipe_base: &str,
    proxy: EventLoopProxy<()>,
    cmd_tx: Sender<ApiCommand>,
) -> std::io::Result<(PipeServer, String)> {
    let name = gmux_pipe::pipe_name_for_user(pipe_base);
    let served_name = name.clone();
    let server = PipeServer::start(&name, move |stream: PipeStream| {
        serve_connection(stream, &proxy, &cmd_tx);
    })?;
    Ok((server, served_name))
}

fn serve_connection(stream: PipeStream, proxy: &EventLoopProxy<()>, cmd_tx: &Sender<ApiCommand>) {
    // Duplicate the handle so BufReader can own the read side while we write independently.
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    loop {
        let request: Request = match read_msg(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return, // client disconnected
            Err(e) => {
                // Protocol error: best-effort error line (id 0 = unknown), then drop.
                let _ = write_msg(&mut writer, &Response::err(0, format!("bad request: {e}")));
                return;
            }
        };
        let id = request.id;
        let (reply_tx, reply_rx): (Sender<Response>, Receiver<Response>) = channel();
        if cmd_tx.send(ApiCommand { request, reply: reply_tx }).is_err() {
            let _ = write_msg(&mut writer, &Response::err(id, "gmux is shutting down"));
            return;
        }
        let _ = proxy.send_event(()); // wake the event loop to service the command
        let response = reply_rx
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|_| Response::err(id, "timed out waiting for gmux"));
        if write_msg(&mut writer, &response).is_err() {
            return;
        }
    }
}

// (BufReader owns the read side; `PipeStream::try_clone` — a DuplicateHandle — provides the
//  independent write side of the same connection.)
