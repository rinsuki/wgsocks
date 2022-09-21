use std::{sync::{Arc, Mutex, mpsc::{self, Receiver, Sender}}, thread::sleep, time::Duration};

use crate::config::{Config, decode_base64_key};

mod recv;
use recv::RxToken;
mod send;
use send::TxToken;

pub struct WGDevice {
    pub socket: std::net::UdpSocket,
    pub tunnel: Arc<Box<boringtun::noise::Tunn>>,
    pub config: Arc<Config>,
    pub recv_rx: Receiver<Vec<u8>>,
    pub send_tx: Sender<Vec<u8>>,
}

impl WGDevice {
    pub fn new<Callback: Send + Fn() -> () + 'static>(config: Arc<Config>, callback: Arc<Mutex<Callback>>) -> WGDevice {
        let socket = {
            // create udp socket
            let local_addr = if config.peer.endpoint.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };

            let socket = std::net::UdpSocket::bind(local_addr).unwrap();
            socket.connect(config.peer.endpoint).unwrap();
            socket
        };
        
        let tunnel = Arc::new(boringtun::noise::Tunn::new(
            x25519_dalek::StaticSecret::from(decode_base64_key(&config.private_key)),
            x25519_dalek::PublicKey::from(decode_base64_key(&config.peer.public_key)),
            None, 
            None, 0, None
        ).unwrap());

        let (recv_tx, recv_rx) = mpsc::channel::<Vec<u8>>();
        { // recv from wireguard
            let recv_socket = socket.try_clone().unwrap();
            let recv_tunnel = tunnel.clone();
            std::thread::spawn(move || {
                loop {
                    let mut buffer = vec![0; 1500];
                    let len = recv_socket.recv(&mut buffer).unwrap();
                    let mut decap_buf = vec![0u8; len];
                    let mut buffer = &buffer[..len];
                    loop {
                        let decap_result = recv_tunnel.decapsulate(None, &buffer, &mut decap_buf);
                        match decap_result {
                            boringtun::noise::TunnResult::WriteToNetwork(data) => {
                                recv_socket.send(data).unwrap();
                                buffer = &[];
                            }
                            boringtun::noise::TunnResult::WriteToTunnelV4(data, _from_addr) => {
                                recv_tx.send(data.to_vec()).unwrap();
                                callback.lock().unwrap()();
                            },
                            boringtun::noise::TunnResult::Done => {
                                break;
                            },
                            boringtun::noise::TunnResult::Err(boringtun::noise::errors::WireGuardError::InvalidCounter) => {
                                // ????
                                break;
                            },
                            boringtun::noise::TunnResult::Err(err) => {
                                println!("Error: {:?}", err);
                                break;
                            }
                            _ => {
                                println!("decap_{:?}", decap_result);
                            },
                        }
                    }
                }
            });
        }

        let (send_tx, send_rx) = mpsc::channel::<Vec<u8>>();
        { // send to wireguard
            let send_socket = socket.try_clone().unwrap();
            let send_tunnel = tunnel.clone();
            std::thread::spawn(move || {
                loop {
                    let data = send_rx.recv().unwrap();
                    let mut wg_buf = vec![0u8; std::cmp::max(data.len() + 32, 148)];
                    let encap_result = send_tunnel.encapsulate(&data, &mut wg_buf);
                    match encap_result {
                        boringtun::noise::TunnResult::WriteToNetwork(data) => {
                            // println!("sending...");
                            send_socket.send(data).unwrap();
                        },
                        _ => {
                            println!("send_{:?}", encap_result);
                        },
                    }
                }
            });
        }

        {
            let tunnel = tunnel.clone();
            let socket = socket.try_clone().unwrap();
            std::thread::spawn(move || {
                let mut buf = vec![0; 1024];
                loop {
                    let result = tunnel.update_timers(&mut buf);
                    match result {
                        boringtun::noise::TunnResult::WriteToNetwork(data) => {
                            println!("timer_write");
                            socket.send(data).unwrap();
                        }
                        boringtun::noise::TunnResult::WriteToTunnelV4(data, _from_addr) => {
                            todo!()
                        },
                        boringtun::noise::TunnResult::Done => {
                            // println!("timer_done");
                        },
                        boringtun::noise::TunnResult::Err(boringtun::noise::errors::WireGuardError::InvalidCounter) => {
                            // ????
                            println!("timer_invalid_counter??");
                        },
                        boringtun::noise::TunnResult::Err(err) => {
                            println!("timer_Error: {:?}", err);
                            // break;
                        }
                        _ => {
                            println!("timer_{:?}", result);
                        },
                    }
                    sleep(Duration::from_secs(1));
                }
            });
        }

        WGDevice { socket, tunnel, config, recv_rx, send_tx }
    }
}

impl<'a> smoltcp::phy::Device<'a> for WGDevice {
    type RxToken = RxToken;
    type TxToken = TxToken<'a>;

    fn receive(&'a mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        let locked = self.recv_rx.try_recv();
        match locked {
            Ok(buffer) => {
                Some((RxToken { buffer }, TxToken { device: self }))
            },
            Err(_) => None,
        }
    }

    fn transmit(&'a mut self) -> Option<Self::TxToken> {
        Some(TxToken { device: self })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.medium = smoltcp::phy::Medium::Ip;
        caps.max_transmission_unit = self.config.mtu;
        caps
    }
}
