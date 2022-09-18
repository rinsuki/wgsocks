use std::{vec, collections::{HashMap, HashSet}, sync::{Arc, Mutex, mpsc::{self}}, time::Duration, io::{Write, Read}};

use boringtun::{self, noise::TunnResult};

mod config;
use config::{Config};

mod wg_device;
use rand::Rng;
use wg_device::WGDevice;

mod socks;

pub enum Queue {
    CreateTCPConnection(std::net::TcpStream, smoltcp::wire::IpAddress, u16),
    ReceiveFromProxyClient(smoltcp::iface::SocketHandle, Vec<u8>),
    DisconnectFromProxyClient(smoltcp::iface::SocketHandle),
    ReceiveFromBoringTun(),
    ForcePoll,
}

fn current_time() -> smoltcp::time::Instant {
    // TODO: use wont rewind clock instead of system time (that might be rewind it)
    smoltcp::time::Instant::now()
}

fn main() {
    let config: Config = serde_json::from_reader(std::fs::File::open("config.json").unwrap()).unwrap();
    let (tx, rx) = mpsc::channel::<Queue>();
    let tx = Arc::new(Mutex::new(tx));

    let device = {
        let tx = tx.clone();
        WGDevice::new(config.clone(), Arc::new(Mutex::new(move || {
            tx.lock().unwrap().send(Queue::ReceiveFromBoringTun()).unwrap();
        })))
    };
    // initialize connection
    {
        let mut buf = vec![0u8; 1500];
        let result = device.tunnel.format_handshake_initiation(&mut buf, false);
        match result {
            TunnResult::WriteToNetwork(data) => {
                device.socket.send(data).unwrap();
            },
            _ => {
                println!("init_{:?}", result);
            },
        }
    }

    let mut iface = smoltcp::iface::InterfaceBuilder::new(device, vec![])
        .ip_addrs([smoltcp::wire::IpCidr::new({
            let octets = config.ip4.octets();
            smoltcp::wire::IpAddress::v4(octets[0], octets[1], octets[2], octets[3])
        }, 32)])
        .finalize();

    {
        let tx = tx.clone();
        std::thread::spawn(move || socks::run_socks_server(tx));
    }

    // None => it shouldn't block
    // something => block w/ timeout
    let mut should_block: Option<smoltcp::time::Instant> = None;
    let mut not_connected_handles = HashSet::new();
    let mut connection_map = HashMap::new();
    let mut rng = rand::thread_rng();

    'queueinfinityloop: loop {
        loop {
            let queue = {
                match should_block {
                    None => {
                        let queue = rx.try_recv();
                        match queue {
                            Ok(queue) => queue,
                            Err(e) => match e {
                                mpsc::TryRecvError::Empty => break,
                                mpsc::TryRecvError::Disconnected => break 'queueinfinityloop,
                            },
                        }
                    }
                    Some(timeout) => {
                        should_block = None;
                        // todo: replace with recv_timeout w/ iface.poll_at
                        if timeout.total_micros() == 0 {
                            // probably just needs re-poll
                            println!("repoll!");
                            break;
                        }
                        let w = timeout - current_time();
                        println!("timeout: {:?}", w);
                        let queue = rx.recv_timeout(Duration::from_micros(w.total_micros()));
                        match queue {
                            Ok(queue) => queue,
                            Err(e) => match e {
                                mpsc::RecvTimeoutError::Timeout => break,
                                mpsc::RecvTimeoutError::Disconnected => break 'queueinfinityloop,
                            },
                        }
                    }
                }
            };
            match queue {
                Queue::CreateTCPConnection(sock, host, port) => {
                    let smolsock = smoltcp::socket::TcpSocket::new(
                        smoltcp::socket::TcpSocketBuffer::new(vec![0; 1024]),
                        smoltcp::socket::TcpSocketBuffer::new(vec![0; 1024]),
                    );
                    let handle = iface.add_socket(smolsock);
                    let (smolsock, inner) = iface.get_socket_and_context::<smoltcp::socket::TcpSocket>(handle);
                    smolsock.connect(
                        inner, 
                        (host, port), 
                        (smoltcp::wire::IpAddress::Unspecified, rng.gen_range(49152..65535))
                    ).unwrap();
                    connection_map.insert(handle, sock);
                    not_connected_handles.insert(handle);
                },
                Queue::ReceiveFromProxyClient(handle, data) => {
                    let smolsock = iface.get_socket::<smoltcp::socket::TcpSocket>(handle);
                    if smolsock.can_send() {
                        match smolsock.send_slice(&data) {
                            Ok(size) => {
                                if size != data.len() {
                                    let tx = tx.lock().unwrap();
                                    tx.send(Queue::ForcePoll).unwrap();
                                    tx.send(Queue::ReceiveFromProxyClient(handle, data[size..].to_vec())).unwrap();
                                }
                            },
                            Err(e) => {
                                println!("send_slice: {:?}", e);
                            },
                        }
                    } else {
                        let tx = tx.lock().unwrap();
                        tx.send(Queue::ForcePoll).unwrap();
                        tx.send(Queue::ReceiveFromProxyClient(handle, data)).unwrap();
                    }
                },
                Queue::DisconnectFromProxyClient(handle) => {
                    let sock = iface.get_socket::<smoltcp::socket::TcpSocket>(handle);
                    sock.close();
                },
                Queue::ReceiveFromBoringTun() => break,
                Queue::ForcePoll => break,
            }
        }
        let poll_res = iface.poll(current_time());
        let readiness_changed = match poll_res {
            Ok(readiness_changed) => readiness_changed,
            Err(e) => {
                match e {
                    smoltcp::Error::Unrecognized => {
                        // unrecognized should be ignored
                    },
                    _ => {
                        println!("poll error: {:?}", e);
                    }
                };
                false
            },
        };
        if readiness_changed {
            // 何かが変わったかもしれないので見回りする
            for (handle, socket) in &connection_map {
                let mut socket = socket;
                let smolsock = iface.get_socket::<smoltcp::socket::TcpSocket>(*handle);
                if smolsock.is_open() {
                    if not_connected_handles.contains(&handle) {
                        socket.write(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).unwrap();
                        let handle = handle.clone();
                        let mut socket = socket.try_clone().unwrap();
                        let tx = tx.clone();
                        std::thread::spawn(move || {
                            loop {
                                let mut buf = vec![0; 1024];
                                let len = socket.read(&mut buf).unwrap();
                                if len == 0 {
                                    break;
                                }
                                println!("read: {:?}", &buf[..len]);
                                tx.lock().unwrap().send(Queue::ReceiveFromProxyClient(handle, buf[..len].to_vec())).unwrap();
                            }
                            tx.lock().unwrap().send(Queue::DisconnectFromProxyClient(handle)).unwrap();
                        });
                        not_connected_handles.remove(&handle);
                    }
                }
                if smolsock.may_recv() {
                    let result = smolsock.recv(|buf| {
                        match socket.write(buf) {
                            Ok(n) => (n, None),
                            Err(e) => {
                                println!("write error: {:?}", e);
                                (0, Some(e))
                            },
                        }
                    });
                    match result {
                        Ok(n) => match n {
                            Some(_) => todo!(),
                            None => {},
                        },
                        Err(e) => {
                            println!("recv error: {:?}", e);
                        },
                    }
                }
            }
        }
        match iface.poll_at(current_time()) {
            Some(next_time) => {
                println!("next_time: {:?}", next_time.total_micros());
                should_block = Some(next_time);
            },
            None => {
                should_block = Some(current_time() + smoltcp::time::Duration::from_secs(1));
            },
        }
    }
}
