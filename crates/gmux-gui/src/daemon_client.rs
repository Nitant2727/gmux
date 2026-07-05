//! Blocking client to the gmux daemon over the named pipe. The thin-client GUI uses this to fetch
//! layout/grids for rendering and to send input/control. If no daemon is answering, it spawns one
//! (`gmux --daemon`) with `CREATE_NO_WINDOW` so the daemon gets a hidden console — required for its
//! ConPTY panes to bind their stdio (the M0 console-binding finding).

use std::io::{self, BufReader};
use std::os::windows::process::CommandExt;
use std::time::Duration;

use gmux_pipe::PipeStream;
use gmux_proto::{read_msg, write_msg, Call, Request, Response, ResultBody};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct DaemonClient {
    reader: BufReader<PipeStream>,
    writer: PipeStream,
    next_id: u64,
}

impl DaemonClient {
    /// Connect to `\\.\pipe\<base>.<user>`, spawning `gmux --daemon` if nothing is listening yet.
    pub fn connect_or_spawn(pipe_base: &str) -> io::Result<DaemonClient> {
        let name = gmux_pipe::pipe_name_for_user(pipe_base);
        if let Ok(stream) = gmux_pipe::client_connect(&name) {
            return DaemonClient::from_stream(stream);
        }
        spawn_daemon();
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(150));
            if let Ok(stream) = gmux_pipe::client_connect(&name) {
                return DaemonClient::from_stream(stream);
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "could not reach or start the gmux daemon"))
    }

    fn from_stream(stream: PipeStream) -> io::Result<DaemonClient> {
        let writer = stream.try_clone()?;
        Ok(DaemonClient { reader: BufReader::new(stream), writer, next_id: 1 })
    }

    /// Send one request and await its response.
    pub fn call(&mut self, call: Call) -> Result<ResultBody, String> {
        let id = self.next_id;
        self.next_id += 1;
        write_msg(&mut self.writer, &Request { id, call }).map_err(|e| e.to_string())?;
        match read_msg::<Response>(&mut self.reader) {
            Ok(Some(r)) => {
                if let Some(e) = r.error {
                    return Err(e);
                }
                r.result.ok_or_else(|| "empty response".to_string())
            }
            Ok(None) => Err("daemon disconnected".to_string()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Fire-and-forget a control call, ignoring the (Done) result.
    pub fn control(&mut self, call: Call) {
        let _ = self.call(call);
    }
}

fn spawn_daemon() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .arg("--daemon")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
}
