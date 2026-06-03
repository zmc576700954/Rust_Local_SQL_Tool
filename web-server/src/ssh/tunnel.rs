/// SSH tunnel types and logic for proxying database connections through SSH.

#[derive(Clone)]
pub struct SshTunnelConfig {
    pub ssh_host: String,
    pub ssh_port: u16,
    pub ssh_username: String,
    pub ssh_password: String,
    pub remote_host: String,
    pub remote_port: u16,
}

pub struct SshTunnelHandle {
    pub local_port: u16,
    stop_tx: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SshTunnelHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn write_all_to_stream_nonblocking(
    stream: &mut std::net::TcpStream,
    data: &[u8],
) -> Result<(), String> {
    let mut written = 0usize;
    while written < data.len() {
        match std::io::Write::write(stream, &data[written..]) {
            Ok(0) => return Err("local stream closed".to_string()),
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(e) => return Err(format!("write local stream failed: {e}")),
        }
    }
    Ok(())
}

fn write_all_to_channel_nonblocking(
    channel: &mut ssh2::Channel,
    data: &[u8],
) -> Result<(), String> {
    let mut written = 0usize;
    while written < data.len() {
        match std::io::Write::write(channel, &data[written..]) {
            Ok(0) => return Err("ssh channel closed".to_string()),
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(e) => return Err(format!("write ssh channel failed: {e}")),
        }
    }
    Ok(())
}

fn proxy_one_connection(
    session: &mut ssh2::Session,
    mut local_stream: std::net::TcpStream,
    remote_host: &str,
    remote_port: u16,
    stop_rx: &std::sync::mpsc::Receiver<()>,
) -> Result<(), String> {
    session.set_blocking(false);
    let mut channel = session
        .channel_direct_tcpip(remote_host, remote_port, None)
        .map_err(|e| format!("open ssh channel failed: {e}"))?;
    local_stream
        .set_nonblocking(true)
        .map_err(|e| format!("set local nonblocking failed: {e}"))?;

    let mut uplink_buf = [0u8; 16 * 1024];
    let mut downlink_buf = [0u8; 16 * 1024];

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        let mut progressed = false;

        match std::io::Read::read(&mut local_stream, &mut uplink_buf) {
            Ok(0) => break,
            Ok(n) => {
                write_all_to_channel_nonblocking(&mut channel, &uplink_buf[..n])?;
                progressed = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("read local stream failed: {e}")),
        }

        match std::io::Read::read(&mut channel, &mut downlink_buf) {
            Ok(0) => break,
            Ok(n) => {
                write_all_to_stream_nonblocking(&mut local_stream, &downlink_buf[..n])?;
                progressed = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("read ssh channel failed: {e}")),
        }

        if !progressed {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    let _ = channel.close();
    let _ = channel.wait_close();
    Ok(())
}

pub fn start_ssh_tunnel(cfg: SshTunnelConfig) -> Result<SshTunnelHandle, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("bind local tunnel failed: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set listener nonblocking failed: {e}"))?;
    let local_port = listener
        .local_addr()
        .map_err(|e| format!("read local tunnel addr failed: {e}"))?
        .port();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    let worker = std::thread::spawn(move || {
        let ssh_tcp = match std::net::TcpStream::connect((cfg.ssh_host.as_str(), cfg.ssh_port)) {
            Ok(v) => v,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("ssh tcp connect failed: {e}")));
                return;
            }
        };
        let _ = ssh_tcp.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let _ = ssh_tcp.set_write_timeout(Some(std::time::Duration::from_secs(10)));

        let mut session = match ssh2::Session::new() {
            Ok(v) => v,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("create ssh session failed: {e}")));
                return;
            }
        };
        session.set_tcp_stream(ssh_tcp);
        if let Err(e) = session.handshake() {
            let _ = ready_tx.send(Err(format!("ssh handshake failed: {e}")));
            return;
        }
        if let Err(e) = session.userauth_password(&cfg.ssh_username, &cfg.ssh_password) {
            let _ = ready_tx.send(Err(format!("ssh auth failed: {e}")));
            return;
        }
        if !session.authenticated() {
            let _ = ready_tx.send(Err("ssh auth failed: unauthenticated".to_string()));
            return;
        }
        let _ = ready_tx.send(Ok(()));

        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((local_stream, _)) => {
                    let _ = proxy_one_connection(
                        &mut session,
                        local_stream,
                        &cfg.remote_host,
                        cfg.remote_port,
                        &stop_rx,
                    );
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let ready = ready_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| "ssh tunnel init timeout".to_string())?;
    ready?;

    Ok(SshTunnelHandle {
        local_port,
        stop_tx,
        worker: Some(worker),
    })
}
