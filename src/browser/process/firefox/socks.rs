//! Bounded native TCP egress. The listener belongs to the isolated network
//! namespace; only its inherited descriptor is available in the host Agent.
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::{JoinHandle, JoinSet},
};

const MAX_CONNECTIONS: usize = 32;
const HANDSHAKE: Duration = Duration::from_secs(3);
const CONNECT: Duration = Duration::from_secs(3);

pub(super) struct Egress {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}
impl Egress {
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
    pub async fn finish(mut self) {
        self.stop();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Egress {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn serve(listener: TcpListener) -> Egress {
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = stopped.changed() => break,
                _ = tasks.join_next(), if !tasks.is_empty() => {},
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break; };
                    if tasks.len() == MAX_CONNECTIONS {
                        drop(stream);
                        continue;
                    }
                    tasks.spawn(async move { let _ = relay(stream).await; });
                }
            }
        }
        tasks.shutdown().await;
    });
    Egress {
        stop,
        task: Some(task),
    }
}

async fn destination(stream: &mut TcpStream) -> io::Result<(String, u16)> {
    let mut greeting = [0; 2];
    stream.read_exact(&mut greeting).await?;
    if greeting[0] != 5 || greeting[1] == 0 {
        return Err(super::other("bidi_socks_greeting"));
    }
    let mut methods = vec![0; greeting[1] as usize];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        stream.write_all(&[5, 255]).await?;
        return Err(super::other("bidi_socks_auth"));
    }
    stream.write_all(&[5, 0]).await?;
    let mut request = [0; 4];
    stream.read_exact(&mut request).await?;
    if request[..3] != [5, 1, 0] {
        return Err(super::other("bidi_socks_tcp_only"));
    }
    let host = match request[3] {
        1 => {
            let mut bytes = [0; 4];
            stream.read_exact(&mut bytes).await?;
            IpAddr::V4(Ipv4Addr::from(bytes)).to_string()
        }
        4 => {
            let mut bytes = [0; 16];
            stream.read_exact(&mut bytes).await?;
            IpAddr::V6(Ipv6Addr::from(bytes)).to_string()
        }
        3 => {
            let length = stream.read_u8().await? as usize;
            if length == 0 {
                return Err(super::other("bidi_socks_host"));
            }
            let mut bytes = vec![0; length];
            stream.read_exact(&mut bytes).await?;
            let host = String::from_utf8(bytes).map_err(super::other)?;
            if !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
            {
                return Err(super::other("bidi_socks_host"));
            }
            host
        }
        _ => return Err(super::other("bidi_socks_address")),
    };
    let port = stream.read_u16().await?;
    if port == 0 {
        return Err(super::other("bidi_socks_port"));
    }
    Ok((host, port))
}

async fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let addresses = tokio::net::lookup_host((host, port)).await?;
    for address in addresses.take(16) {
        if let Ok(stream) = TcpStream::connect(address).await {
            return Ok(stream);
        }
    }
    Err(super::other("bidi_egress_unreachable"))
}

async fn relay(mut client: TcpStream) -> io::Result<()> {
    let (host, port) = tokio::time::timeout(HANDSHAKE, destination(&mut client))
        .await
        .map_err(super::other)??;
    let result = tokio::time::timeout(CONNECT, connect(&host, port)).await;
    let mut upstream = match result {
        Ok(Ok(stream)) => stream,
        _ => {
            let _ = client.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            return Err(super::other("bidi_egress_unreachable"));
        }
    };
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    // Fixed 8-KiB buffers in each direction. Session stop aborts every task;
    // long-lived website connections cannot create unbounded tasks or buffers.
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::fd::AsRawFd, os::unix::fs::MetadataExt};

    #[tokio::test]
    async fn connection_limit_refuses_extra_clients_and_shutdown_closes_every_owner() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let inode = fs::metadata(format!("/proc/self/fd/{}", listener.as_raw_fd()))
            .unwrap()
            .ino()
            .to_string();
        // The process must release its own descriptor as soon as finish returns.
        let socket = std::path::PathBuf::from(format!("socket:[{inode}]"));
        let process_owns_listener = || {
            fs::read_dir("/proc/self/fd").unwrap().any(|entry| {
                match fs::read_link(entry.unwrap().path()) {
                    Ok(target) => target == socket,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => panic!("cannot inspect process descriptor: {error}"),
                }
            })
        };
        let listener_is_open = || {
            fs::read_to_string("/proc/self/net/tcp")
                .unwrap()
                .lines()
                .any(|row| row.split_whitespace().nth(9) == Some(inode.as_str()))
        };
        assert!(process_owns_listener() && listener_is_open());
        let server = serve(listener);
        let mut clients = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            let mut client = TcpStream::connect(address).await.unwrap();
            client.write_all(&[5, 1, 0]).await.unwrap();
            let mut accepted = [0; 2];
            client.read_exact(&mut accepted).await.unwrap();
            assert_eq!(accepted, [5, 0]);
            clients.push(client);
        }
        let mut extra = TcpStream::connect(address).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), extra.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        server.finish().await;
        assert!(
            !process_owns_listener(),
            "process still owns egress listener {inode}"
        );
        for mut client in clients {
            assert_eq!(client.read(&mut byte).await.unwrap(), 0);
        }
        // A concurrently forked child can briefly hold the close-on-exec fd.
        // The namespace must also release it, including any inherited copies.
        tokio::time::timeout(Duration::from_secs(1), async {
            while listener_is_open() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("egress listener remained open in the network namespace");
    }
}
