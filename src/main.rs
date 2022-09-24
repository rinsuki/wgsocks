use core::panic;
use std::{vec, collections::{HashMap, HashSet}, sync::{Arc, Mutex, atomic::AtomicBool}, time::Duration, io::{Write, Read}, net::{Shutdown, SocketAddr}};

use boringtun::{self, noise::TunnResult};

mod config;
use config::{Config};

mod wg_device;
use rand::{seq::SliceRandom};
use socks::ClientSocket;
use wg_device::WGDevice;

mod socks;

#[derive(Debug)]
pub enum Queue {
    CreateTCPConnection(tokio::sync::oneshot::Sender<socks::OpenSocketResponse>, smoltcp::wire::IpAddress, u16),
    LinkSocket(smoltcp::iface::SocketHandle, socks::ClientSocket),
    ReceiveFromProxyClient(smoltcp::iface::SocketHandle, Vec<u8>, tokio::sync::mpsc::Sender<()>),
    DisconnectFromProxyClient(smoltcp::iface::SocketHandle),
    ReceiveFromBoringTun(),
    ForcePoll(u64),
}

fn current_time() -> smoltcp::time::Instant {
    // TODO: use wont rewind clock instead of system time (that might be rewind it)
    smoltcp::time::Instant::now()
}

#[tokio::main]
async fn main() {
    let config = Arc::new(serde_json::from_reader::<_, Config>(std::fs::File::open("config.json").unwrap()).unwrap());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let mut port_queue = tokio::sync::mpsc::channel::<u16>(16384);
    {
        let mut ports = (49152..65535).collect::<Vec<_>>();
        let mut t = rand::thread_rng();
        ports.shuffle(&mut t);
        for port in ports {
            port_queue.0.send(port).await;
        }
    }

    let device = {
        let tx = tx.clone();
        WGDevice::new(config.clone(), Arc::new(Mutex::new(move || {
            tx.send(Queue::ReceiveFromBoringTun()).unwrap();
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
        tokio::spawn(async move {
            socks::run_socks_server(tx, config).await;
        });
    }

    // None => it shouldn't block
    // something => block w/ timeout
    let mut should_block: Option<smoltcp::time::Instant> = None;
    let mut currently_connecting_handles = HashMap::new();
    // let mut client_disconnected_handles = HashSet::new();
    let mut connection_map: HashMap<smoltcp::iface::SocketHandle, ClientSocket> = HashMap::new();
    let mut connection_port_map = HashMap::new();
    let mut cnt = 0 as u64;
    let mut check_poll_at = true;
    let dump_current_queue = Arc::new(AtomicBool::new(false));
    // check WGSOCKS_DEBUG_QUEUE
    let debug_queue_mode = std::env::var("WGSOCKS_DEBUG_QUEUE").is_ok();

    signal_hook::flag::register(signal_hook::consts::SIGUSR1, dump_current_queue.clone()).unwrap();

    'queueinfinityloop: loop {
        cnt += 1;
        if dump_current_queue.swap(false, std::sync::atomic::Ordering::Relaxed) {
            let mut queues = vec![];
            println!("--- QUEUE DUMP ---");
            loop {
                match rx.try_recv() {
                    Ok(queue) => {
                        println!("{:?}", queue);
                        queues.push(queue);
                    },
                    Err(_) => {
                        break;
                    },
                }
            }
            for queue in queues {
                tx.send(queue).unwrap();
            }
            println!("--- END QUEUE DUMP ---");
            println!("--- CONNECTION STATUSES DUMP ---");
            for (handle, socket) in connection_map.iter() {
                let smolsock = iface.get_socket::<smoltcp::socket::TcpSocket>(*handle);
                println!("{}: {} -> {} -> {} (State: {})", handle, socket.peer, smolsock.local_endpoint(), smolsock.remote_endpoint(), smolsock.state());
            }
            println!("--- END CONNECTION STATUSES DUMP ---");
        }

        let mut have_force_poll = false;

        if debug_queue_mode {
            println!("START QUEUE LOOP (should_block={:?})", should_block);
        }

        loop {
            let queue = {
                match should_block {
                    None => {
                        let queue = rx.try_recv();
                        match queue {
                            Ok(queue) => queue,
                            Err(e) => match e {
                                tokio::sync::mpsc::error::TryRecvError::Empty => {
                                    check_poll_at = true;
                                    break;
                                },
                                tokio::sync::mpsc::error::TryRecvError::Disconnected => break 'queueinfinityloop,
                            },
                        }
                    }
                    Some(timeout) => {
                        should_block = None;
                        // todo: replace with recv_timeout w/ iface.poll_at
                        let w = timeout - current_time();
                        // println!("timeout: {:?}", w);
                        let queue = tokio::time::timeout(Duration::from_micros(w.total_micros()), rx.recv()).await;
                        match queue {
                            Ok(Some(queue)) => queue,
                            Ok(None) => {
                                panic!("queue channel closed");
                            },
                            Err(_) => break,
                        }
                    }
                }
            };
            if debug_queue_mode {
                println!("QUEUE: {:?}", queue);
            }
            match queue {
                Queue::CreateTCPConnection(tx, host, port) => {
                    let local_port = match port_queue.1.try_recv() {
                        Ok(p) => p,
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                            println!("no more ports");
                            tx.send(socks::OpenSocketResponse::FailureNoPort).unwrap();
                            continue
                        },
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => panic!("port queue disconnected"),
                    };
                    let smolsock = smoltcp::socket::TcpSocket::new(
                        smoltcp::socket::TcpSocketBuffer::new(vec![0; 16384]),
                        smoltcp::socket::TcpSocketBuffer::new(vec![0; 16384]),
                    );
                    let handle = iface.add_socket(smolsock);
                    let (smolsock, inner) = iface.get_socket_and_context::<smoltcp::socket::TcpSocket>(handle);
                    smolsock.connect(
                        inner, 
                        (host, port), 
                        (smoltcp::wire::IpAddress::Unspecified, local_port),
                    ).unwrap();
                    connection_port_map.insert(handle, local_port);
                    currently_connecting_handles.insert(handle, tx);
                },
                Queue::LinkSocket(handle, socket) => {
                    connection_map.insert(handle, socket);
                }
                Queue::ReceiveFromProxyClient(handle, data, tx2) => {
                    let smolsock = iface.get_socket::<smoltcp::socket::TcpSocket>(handle);
                    if smolsock.can_send() {
                        match smolsock.send_slice(&data) {
                            Ok(size) => {
                                if size != data.len() {
                                    tx.send(Queue::ForcePoll(cnt)).unwrap();
                                    tx.send(Queue::ReceiveFromProxyClient(handle, data[size..].to_vec(), tx2)).unwrap();
                                } else {
                                    tx2.send(()).await.unwrap();
                                }
                            },
                            Err(e) => {
                                println!("send_slice: {:?}", e);
                            },
                        }
                    } else if smolsock.state() != smoltcp::socket::TcpState::Closed {
                        if smolsock.may_send() {
                            if !have_force_poll {
                                tx.send(Queue::ForcePoll(cnt)).unwrap();
                                have_force_poll = true;
                            }
                            tx.send(Queue::ReceiveFromProxyClient(handle, data, tx2)).unwrap();
                        } else {
                            println!("cantsend {:?}", smolsock.state());
                        }
                    }
                },
                Queue::DisconnectFromProxyClient(handle) => {
                    if connection_map.contains_key(&handle) {
                        let sock = iface.get_socket::<smoltcp::socket::TcpSocket>(handle);
                        sock.close();
                    } else {
                        // probably already closed from server
                    }
                },
                Queue::ReceiveFromBoringTun() => {
                    if !have_force_poll {
                        tx.send(Queue::ForcePoll(cnt)).unwrap();
                        have_force_poll = true;
                    }
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
            {
                let mut handles = vec![];
                for handle in currently_connecting_handles.keys() {
                    let smolsock = iface.get_socket::<smoltcp::socket::TcpSocket>(*handle);
                    if smolsock.state() == smoltcp::socket::TcpState::Established {
                        handles.push(*handle);
                    }
                }
                for handle in handles {
                    let tx = currently_connecting_handles.remove(&handle).unwrap();
                    tx.send(socks::OpenSocketResponse::Success(handle)).unwrap();
                }
            }
            for (handle, socket) in &connection_map {
                let smolsock = iface.get_socket::<smoltcp::socket::TcpSocket>(*handle);
                if smolsock.can_recv() {
                    let result = smolsock.recv(|buf| {
                        if buf.len() > 0 {
                            match socket.tx.send(buf.to_vec()) {
                                Ok(_) => (buf.len(), None),
                                Err(err) => {
                                    println!("warning!!! send error...");
                                    (buf.len(), Some(err))
                                },
                            }
                        } else {
                            (buf.len(), None)
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
                    println!("close {}", handle);
                    _ = socket.tx.send(vec![]);
                    disconnected_handles.push(*handle);
                }
            }
            for handle in disconnected_handles {
                let local_port = connection_port_map.remove(&handle).unwrap();
                port_queue.0.send(local_port).await.unwrap();
                connection_map.remove(&handle);
                iface.remove_socket(handle);
            }
        } else {
            check_poll_at = true;
        }
        if check_poll_at {
            match iface.poll_at(current_time()) {
                Some(next_time) => {
                    if next_time.micros() != 0 {
                        should_block = Some(next_time);
                    } else {
                        should_block = None;
                    }
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
