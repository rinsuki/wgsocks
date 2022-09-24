use std::{net::{TcpStream, ToSocketAddrs}, sync::{Arc, Mutex, atomic::AtomicBool}, io::{Read, Write}};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{Queue, config::Config};

pub async fn run_socks_server(tx: tokio::sync::mpsc::UnboundedSender<Queue>, config: Arc<Config>) {
    let socks_listener = tokio::net::TcpListener::bind("0.0.0.0:1080").await.unwrap();
    loop {
        let (socks_socket, _) = socks_listener.accept().await.unwrap();
        let tx = tx.clone();
        let config = config.clone();
        tokio::spawn(async move {
            match handle_socks(socks_socket, config, tx).await {
                Ok(_) => {
                },
                Err(e) => {
                    println!("socks error: {:?}", e);
                },
            }
        });
    }
}

#[derive(Debug)]
pub struct ClientSocket {
    pub peer: std::net::SocketAddr,
    pub addr: smoltcp::wire::IpAddress,
    pub port: u16,
    pub tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Debug)]
pub enum OpenSocketResponse {
    FailureNoPort,
    Success(smoltcp::iface::SocketHandle),
}

// RFC 1928 Server Implementation
async fn handle_socks(socks_socket: tokio::net::TcpStream, config: Arc<Config>, tx: tokio::sync::mpsc::UnboundedSender<Queue>) -> std::io::Result<()> {
    let mut socks_socket = socks_socket;
    // 1. Read the first 2 bytes of the request
    let mut buf = [0u8; 2];
    socks_socket.read_exact(&mut buf).await?;
    // 2. Check that the version is 5
    if buf[0] != 5 {
        panic!("Invalid SOCKS version: {}", buf[0]);
    }
    // 3. Read authentication methods 
    let mut buf = vec![0u8; buf[1] as usize];
    socks_socket.read_exact(&mut buf).await?;
    // 4. Check that the authentication method is 0 (no authentication)
    let mut have_no_auth = false;
    for method in buf {
        if method == 0 {
            have_no_auth = true;
            break;
        }
    }
    if !have_no_auth {
        panic!("No supported authentication method");
    }
    // 5. Send authentication method
    socks_socket.write_all(&[5, 0]).await?;
    // 6. Read request
    let mut buf = [0u8; 4];
    socks_socket.read_exact(&mut buf).await?;
    // 7. Check that the version is 5
    if buf[0] != 5 {
        panic!("Invalid SOCKS version");
    }
    // 8. Check that the command is 1 (CONNECT)
    if buf[1] != 1 {
        panic!("Invalid SOCKS command");
    }
    // 9. Check that the reserved byte is 0
    if buf[2] != 0 {
        panic!("Invalid SOCKS reserved byte");
    }
    // 10. Read address
    let addr: Option<smoltcp::wire::IpAddress> = match buf[3] {
        1 => {
            // IPv4
            let mut buf = [0u8; 4];
            socks_socket.read_exact(&mut buf).await.unwrap();
            Some(smoltcp::wire::IpAddress::v4(buf[0], buf[1], buf[2], buf[3]))
        },
        3 => {
            // Domain name
            match {
                let mut char_count = [0u8; 1];
                socks_socket.read_exact(&mut char_count).await.unwrap();
                let mut buf = vec![0u8; char_count[0] as usize];
                socks_socket.read_exact(&mut buf).await.unwrap();
                match String::from_utf8(buf) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        println!("failed to parse domain name as UTF-8");
                        None
                    },
                }
            } {
                Some(domain_name) => match config.dns {
                    None => {
                        println!("DNS is not configured");
                        None
                    },
                    Some(ref dns) => {
                        match dns {
                            crate::config::DNSConfig::Special(crate::config::SpecialDNSTypes::System) => {
                                println!("using system DNS: {}", &domain_name);
                                let addr = (domain_name + ":443").to_socket_addrs();
                                match addr {
                                    Ok(mut addrs) => (move || {
                                        for addr in addrs {
                                            println!("resolved: {}", addr);
                                            if addr.is_ipv6() {
                                                continue;
                                            }
                                            return Some(smoltcp::wire::IpAddress::from(addr.ip()));
                                        }
                                        return None
                                    })(),
                                    Err(e) => {
                                        println!("failed to resolve domain name: {}", e);
                                        None
                                    },
                                }
                            }
                        }
                    }
                },
                None => None
            }
        },
        4 => {
            println!("Currently IPv6 dest is not supported");
            None
            // // IPv6
            // let mut buf = [0u8; 16];
            // socks_socket.read_exact(&mut buf).unwrap();
            // // ... is that works??
            // addr = Some(smoltcp::wire::IpAddress::v6(
            //     (buf[0] as u16) | ((buf[1] as u16) << 8),
            //     (buf[2] as u16) | ((buf[3] as u16) << 8),
            //     (buf[4] as u16) | ((buf[5] as u16) << 8),
            //     (buf[6] as u16) | ((buf[7] as u16) << 8),
            //     (buf[8] as u16) | ((buf[9] as u16) << 8),
            //     (buf[10] as u16) | ((buf[11] as u16) << 8),
            //     (buf[12] as u16) | ((buf[13] as u16) << 8),
            //     (buf[14] as u16) | ((buf[15] as u16) << 8),
            // ))
        },
        _ => panic!("Invalid SOCKS address type: {}", buf[3]),
    };
    let addr = match addr {
        Some(addr) => addr,
        None => {
            socks_socket.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid address"));
        },
    };
    // 12. Connect to address
    let port = {
        let mut buf = [0u8; 2];
        socks_socket.read_exact(&mut buf).await.unwrap();
        u16::from_be_bytes(buf)
    };
    
    let handle = {
        let (otx, orx) = tokio::sync::oneshot::channel();
        tx.send(Queue::CreateTCPConnection(otx, addr, port)).unwrap();
        match orx.await.unwrap() {
            OpenSocketResponse::FailureNoPort => {
                socks_socket.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "Failed to connect"));
            },
            OpenSocketResponse::Success(handle) => handle,
        }
    };

    let (packet_tx, mut packet_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (send_tx, mut send_rx) = tokio::sync::mpsc::channel(1);
    let peer = socks_socket.peer_addr().unwrap();
    let (mut read_socket, mut send_socket) = socks_socket.into_split();

    let client_socket = ClientSocket{
        tx: packet_tx,
        peer,
        addr,
        port,
    };
    
    tx.send(Queue::LinkSocket(handle, client_socket)).unwrap();

    let server_closed = Arc::new(AtomicBool::new(false));

    {
        // socks client -> smoltcp
        let tx = tx.clone();
        send_tx.send(()).await.unwrap();
        let server_closed = server_closed.clone();
        tokio::spawn(async move {
            loop {
                match send_rx.recv().await {
                    Some(_) => {},
                    None => break,
                }
                let mut buf = [0u8; 1024];
                let n = match read_socket.read(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        println!("failed to read from socket: {}", e);
                        break;
                    },
                };
                if n == 0 {
                    break;
                }
                let buf = &buf[..n];
                if server_closed.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                match tx.send(Queue::ReceiveFromProxyClient(handle, buf.to_vec(), send_tx.clone())) {
                    Ok(_) => (),
                    Err(_) => {
                        break
                    },
                };
            }
            tx.send(Queue::DisconnectFromProxyClient(handle)).unwrap();
        });
    }
    
    {
        // smoltcp -> socks client
        let tx = tx.clone();
        tokio::spawn(async move {
            send_socket.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await.unwrap();
            loop {
                match packet_rx.recv().await {
                    Some(packet) => {
                        if packet.len() == 0 {
                            server_closed.store(true, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                        match send_socket.write_all(&packet).await {
                            Ok(_) => (),
                            Err(e) => {
                                println!("failed to write to socket: {}", e);
                                break;
                            },
                        }
                    },
                    None => break,
                }
            }
            server_closed.store(true, std::sync::atomic::Ordering::Relaxed);
            tx.send(Queue::DisconnectFromProxyClient(handle)).unwrap();
        });
    }

    Ok(())
}