use std::{vec, collections::{HashMap, HashSet}, sync::{Arc, Mutex, mpsc::{self}}, time::Duration, io::{Write, Read}, net::Shutdown};

use boringtun::{self, noise::TunnResult};

mod config;
use config::{Config};

mod wg_device;
use rand::{seq::SliceRandom};
use wg_device::WGDevice;

mod socks;

pub enum Queue {
    CreateTCPConnection(std::net::TcpStream, smoltcp::wire::IpAddress, u16),
    ReceiveFromProxyClient(smoltcp::iface::SocketHandle, Vec<u8>, mpsc::Sender<()>),
    DisconnectFromProxyClient(smoltcp::iface::SocketHandle),
    ReceiveFromBoringTun(),
    ForcePoll(u64),
}

fn current_time() -> smoltcp::time::Instant {
    // TODO: use wont rewind clock instead of system time (that might be rewind it)
    smoltcp::time::Instant::now()
}

fn main() {
    let config = Arc::new(serde_json::from_reader::<_, Config>(std::fs::File::open("config.json").unwrap()).unwrap());
    let (tx, rx) = mpsc::channel::<Queue>();
    let tx = Arc::new(Mutex::new(tx));

    let port_queue = mpsc::channel::<u16>();
    {
        let mut ports = (49152..65535).collect::<Vec<_>>();
        let mut t = rand::thread_rng();
        ports.shuffle(&mut t);
        for port in ports {
            port_queue.0.send(port).unwrap();
        }
    }

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
        std::thread::spawn(move || socks::run_socks_server(tx, config));
    }

    // None => it shouldn't block
    // something => block w/ timeout
    let mut should_block: Option<smoltcp::time::Instant> = None;
    let mut not_connected_handles = HashSet::new();
    let mut client_disconnected_handles = HashSet::new();
    let mut connection_map = HashMap::new();
    let mut connection_port_map = HashMap::new();
    let mut cnt = 0 as u64;
    let mut check_poll_at = true;

    'queueinfinityloop: loop {
        cnt += 1;
        loop {
            let queue = {
                match should_block {
                    None => {
                        let queue = rx.try_recv();
                        match queue {
                            Ok(queue) => queue,
                            Err(e) => match e {
                                mpsc::TryRecvError::Empty => {
                                    check_poll_at = true;
                                    break;
                                },
                                mpsc::TryRecvError::Disconnected => break 'queueinfinityloop,
                            },
                        }
                    }
                    Some(timeout) => {
                        should_block = None;
                        // todo: replace with recv_timeout w/ iface.poll_at
                        if timeout.total_micros() == 0 {
                            // probably just needs re-poll
                            // println!("repoll");
                            break;
                        }
                        let w = timeout - current_time();
                        // println!("timeout: {:?}", w);
                        let queue = rx.recv_timeout(Duration::from_micros(w.total_micros()));
                        match queue {
                            Ok(queue) => queue,
                            Err(e) => match e {
                                mpsc::RecvTimeoutError::Timeout => {
                                    check_poll_at = true;
                                    break;
                                },
                                mpsc::RecvTimeoutError::Disconnected => break 'queueinfinityloop,
                            },
                        }
                    }
                }
            };
            match queue {
                Queue::CreateTCPConnection(sock, host, port) => {
                    let smolsock = smoltcp::socket::TcpSocket::new(
                        smoltcp::socket::TcpSocketBuffer::new(vec![0; 16384]),
                        smoltcp::socket::TcpSocketBuffer::new(vec![0; 16384]),
                    );
                    let handle = iface.add_socket(smolsock);
                    let (smolsock, inner) = iface.get_socket_and_context::<smoltcp::socket::TcpSocket>(handle);
                    let local_port = port_queue.1.recv().unwrap();
                    smolsock.connect(
                        inner, 
                        (host, port), 
                        (smoltcp::wire::IpAddress::Unspecified, local_port),
                    ).unwrap();
                    connection_map.insert(handle, sock);
                    connection_port_map.insert(handle, port);
                    not_connected_handles.insert(handle);
                },
                Queue::ReceiveFromProxyClient(handle, data, tx2) => {
                    let smolsock = iface.get_socket::<smoltcp::socket::TcpSocket>(handle);
                    if smolsock.can_send() {
                        match smolsock.send_slice(&data) {
                            Ok(size) => {
                                if size != data.len() {
                                    let tx = tx.lock().unwrap();
                                    tx.send(Queue::ForcePoll(cnt)).unwrap();
                                    tx.send(Queue::ReceiveFromProxyClient(handle, data[size..].to_vec(), tx2)).unwrap();
                                } else {
                                    tx2.send(());
                                }
                            },
                            Err(e) => {
                                println!("send_slice: {:?}", e);
                            },
                        }
                    } else if smolsock.state() != smoltcp::socket::TcpState::Closed {
                        if smolsock.may_send() {
                            let tx = tx.lock().unwrap();
                            tx.send(Queue::ForcePoll(cnt)).unwrap();
                            tx.send(Queue::ReceiveFromProxyClient(handle, data, tx2)).unwrap();
                        } else {
                            println!("cantsend {:?}", smolsock.state());
                        }
                    }
                },
                Queue::DisconnectFromProxyClient(handle) => {
                    let sock = iface.get_socket::<smoltcp::socket::TcpSocket>(handle);
                    sock.close();
                },
                Queue::ReceiveFromBoringTun() => {
                    tx.lock().unwrap().send(Queue::ForcePoll(cnt)).unwrap();
                },
                Queue::ForcePoll(c) => {
                    if cnt == c {
                        check_poll_at = true;
                        break
                    }
                },
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
            let mut disconnected_handles = vec![];
            for (handle, mut socket) in &connection_map {
                let smolsock = iface.get_socket::<smoltcp::socket::TcpSocket>(*handle);
                if smolsock.may_recv() {
                    if not_connected_handles.contains(&handle) {
                        println!("open!");
                        socket.write(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).unwrap();
                        let handle = handle.clone();
                        let mut socket = socket.try_clone().unwrap();
                        let tx = tx.clone();
                        std::thread::spawn(move || {
                            let (tx2, rx2) = mpsc::channel();
                            tx2.send(()).unwrap();
                            loop {
                                let mut buf = vec![0; 1024];
                                rx2.recv().unwrap();
                                let len = match socket.read(&mut buf) {
                                    Ok(len) => len,
                                    Err(e) => {
                                        println!("read error: {:?}", e);
                                        break;
                                    },
                                };
                                if len == 0 {
                                    break;
                                }
                                tx.lock().unwrap().send(Queue::ReceiveFromProxyClient(handle, buf[..len].to_vec(), tx2.clone())).unwrap();
                            }
                            tx.lock().unwrap().send(Queue::DisconnectFromProxyClient(handle)).unwrap();
                        });
                        not_connected_handles.remove(&handle);
                    }
                }
                if smolsock.can_recv() && !client_disconnected_handles.contains(handle) {
                    let result = smolsock.recv(|buf| {
                        match socket.write(buf) {
                            Ok(n) => (n, ()),
                            Err(e) => {
                                println!("write error ({:?}): {:?}", handle, e);
                                client_disconnected_handles.insert(handle.clone());
                                tx.lock().unwrap().send(Queue::DisconnectFromProxyClient(*handle)).unwrap();
                                (0, ())
                            },
                        }
                    });
                    match result {
                        Ok(_) => {},
                        Err(e) => {
                            println!("recv error: {:?}", e);
                        },
                    }
                }
                if smolsock.state() == smoltcp::socket::TcpState::Closed {
                    match socket.shutdown(Shutdown::Both) {
                        Ok(_) => {},
                        Err(e) => {
                            println!("shutdown error({:?}): {:?}", handle, e);
                        },
                    };
                    disconnected_handles.push(*handle);
                }
            }
            for handle in disconnected_handles {
                let local_port = connection_port_map.remove(&handle).unwrap();
                port_queue.0.send(local_port).unwrap();
                connection_map.remove(&handle);
                iface.remove_socket(handle);
            }
        }
        if check_poll_at {
            match iface.poll_at(current_time()) {
                Some(next_time) => {
                    should_block = Some(next_time);
                },
                None => {
                    should_block = Some(current_time() + smoltcp::time::Duration::from_secs(1));
                },
            }
            check_poll_at = false;
        } else {
            should_block = None;
        }
    }
}
