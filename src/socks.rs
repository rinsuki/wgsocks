use std::{net::TcpStream, sync::{Arc, Mutex, mpsc::Sender}, io::{Read, Write}};

use crate::{Queue};


pub fn run_socks_server(tx: Arc<Mutex<Sender<Queue>>>) {
    let socks_listener = std::net::TcpListener::bind("0.0.0.0:1080").unwrap();
    let mut threads = vec![];

    for socks_socket in socks_listener.incoming() {
        match socks_socket {
            Ok(socks_socket) => {
                let tx = tx.clone();
                threads.push(std::thread::spawn(move || {
                    match handle_socks(socks_socket) {
                        Ok((socket, addr, port)) => {
                            // notify it
                            tx.lock().unwrap().send(Queue::CreateTCPConnection(socket, addr, port)).unwrap();
                        },
                        Err(e) => {
                            println!("socks error: {:?}", e);
                        },
                    }
                }));
            },
            Err(e) => {
                println!("socks error: {}", e);
                break;
            },
        }
    }

    for thread in threads {
        thread.join().unwrap();
    }
}

// RFC 1928 Server Implementation
fn handle_socks(socks_socket: TcpStream) -> std::io::Result<(TcpStream, smoltcp::wire::IpAddress, u16)> {
    let mut socks_socket = socks_socket;
    // 1. Read the first 2 bytes of the request
    let mut buf = [0u8; 2];
    socks_socket.read_exact(&mut buf)?;
    // 2. Check that the version is 5
    if buf[0] != 5 {
        panic!("Invalid SOCKS version");
    }
    // 3. Read authentication methods 
    let mut buf = vec![0u8; buf[1] as usize];
    socks_socket.read_exact(&mut buf)?;
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
    socks_socket.write(&[5, 0]).unwrap();
    // 6. Read request
    let mut buf = [0u8; 4];
    socks_socket.read_exact(&mut buf).unwrap();
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
            socks_socket.read_exact(&mut buf).unwrap();
            Some(smoltcp::wire::IpAddress::v4(buf[0], buf[1], buf[2], buf[3]))
        },
        3 => {
            // Domain name
            println!("Currently DNS resolve is not supported");
            None
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
            socks_socket.write(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid address"));
        },
    };
    // 12. Connect to address
    let port = {
        let mut buf = [0u8; 2];
        socks_socket.read_exact(&mut buf).unwrap();
        u16::from_be_bytes(buf)
    };
    Ok((socks_socket, addr, port))
}