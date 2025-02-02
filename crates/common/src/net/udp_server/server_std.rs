use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::error::AppError;
use crate::logging::{debug, error, info};
use crate::net::stream_utils;
use crate::target;

const POLL_SERVER_SOCKET_TOKEN: mio::Token = mio::Token(0);
const POLL_DURATION_MSECS: u64 = 1000;

const RECV_BUFFER_SIZE: usize = 64 * 1024;

/// This is a UDP server, which will listen/accept client connections
pub struct Server {
    visitor: Arc<Mutex<dyn ServerVisitor>>,
    _server_port: u16,
    server_socket: Option<UdpSocket>,
    server_addr: SocketAddr,
    polling: bool,
    closing: bool,
    closed: bool,
}

impl Server {
    /// Server constructor
    pub fn new(visitor: Arc<Mutex<dyn ServerVisitor>>, server_port: u16) -> Result<Self, AppError> {
        let server_addr_str = format!("[::]:{}", server_port);
        let server_addr = SocketAddr::from_str(&server_addr_str).map_err(|err| {
            AppError::GenWithMsgAndErr(
                format!(
                    "Failed converting server addr string: addr={}",
                    server_addr_str
                ),
                Box::new(err),
            )
        })?;

        Ok(Self {
            visitor,
            _server_port: server_port,
            server_socket: None,
            server_addr,
            polling: false,
            closing: false,
            closed: false,
        })
    }

    /// Bind/listen on port
    pub fn bind_listener(&mut self) -> Result<(), AppError> {
        let server_socket = UdpSocket::bind(self.server_addr).map_err(|err| {
            AppError::GenWithMsgAndErr(
                format!(
                    "Error binding UDP socket: server_addr={:?}",
                    &self.server_addr
                ),
                Box::new(err),
            )
        })?;
        server_socket.set_nonblocking(true).map_err(|err| {
            AppError::GenWithMsgAndErr(
                format!(
                    "Failed making UDP server socket non-blocking: server_addr={:?}",
                    &self.server_addr
                ),
                Box::new(err),
            )
        })?;

        self.server_socket = Some(server_socket);
        self.closing = false;
        self.closed = false;
        self.polling = false;

        info(
            &target!(),
            &format!("Server started: addr={:?}", &self.server_addr),
        );

        self.visitor.lock().unwrap().on_listening()
    }

    /// Request shutdown for poller and listener
    pub fn shutdown(&mut self) {
        if !self.polling {
            self.perform_shutdown();
        } else {
            self.polling = false;
        }
    }

    /// Get a copy of the server socket
    pub fn clone_server_socket(&self) -> Result<UdpSocket, AppError> {
        match &self.server_socket {
            Some(socket) => socket.try_clone().map_err(|err| {
                AppError::GenWithMsgAndErr(
                    "Failed to clone UDP server socket".to_string(),
                    Box::new(err),
                )
            }),

            None => Err(AppError::General(
                "Server socket not available for cloning".to_string(),
            )),
        }
    }

    /// Request shutdown for poller
    pub fn stop_poller(&mut self) {
        self.polling = false;
    }

    /// Send message to client socket
    pub fn send_message(
        server_socket: &UdpSocket,
        socket_addr: &SocketAddr,
        data: &Vec<u8>,
    ) -> Result<usize, AppError> {
        server_socket
            .send_to(data.as_slice(), socket_addr)
            .map_err(|err| {
                AppError::GenWithMsgAndErr(
                    format!(
                        "Error while sending message on UDP socket: dest={:?}",
                        socket_addr
                    ),
                    Box::new(err),
                )
            })
    }

    /// Shutdown for poller and listener
    fn perform_shutdown(&mut self) {
        self.closing = true;
        self.closed = true;
        self.polling = false;
        self.server_socket = None;

        info(
            &target!(),
            &format!("Server shutdown: server_addr={:?}", &self.server_addr),
        );
    }

    /// Poll and dispatch new incoming messages
    pub fn poll_new_messages(&mut self) -> Result<(), AppError> {
        self.assert_listening()?;

        if self.polling {
            return Err(AppError::General(format!(
                "Already polling for new messages: server_addr={:?}",
                &self.server_addr
            )));
        }

        // Setup MIO poller
        let mut server_socket = mio::net::UdpSocket::from_std(stream_utils::clone_std_udp_socket(
            self.server_socket.as_ref().unwrap(),
        )?);

        // Setup MIO poller registry
        let mut poll: mio::Poll;

        match mio::Poll::new() {
            Ok(_poll) => poll = _poll,
            Err(err) => {
                return Err(AppError::GenWithMsgAndErr(
                    "Error creating new MIO poller".to_string(),
                    Box::new(err),
                ));
            }
        }

        if let Err(err) = poll.registry().register(
            &mut server_socket,
            POLL_SERVER_SOCKET_TOKEN,
            mio::Interest::READABLE,
        ) {
            return Err(AppError::GenWithMsgAndErr(
                "Error registering udp server socket in MIO registry".to_string(),
                Box::new(err),
            ));
        }

        let mut events = mio::Events::with_capacity(256);

        // Start polling loop
        let mut polling_error = None;
        self.polling = true;

        info(
            &target!(),
            &format!(
                "Polling messages started: server_addr={:?}",
                &self.server_addr
            ),
        );

        while self.polling {
            // Poll for server socket message read readiness
            match poll.poll(
                &mut events,
                Some(Duration::from_millis(POLL_DURATION_MSECS)),
            ) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}

                Err(err) => {
                    polling_error = Some(AppError::GenWithMsgAndErr(
                        "Error while polling for IO events".to_string(),
                        Box::new(err),
                    ));
                    self.polling = false;
                }

                Ok(()) => {
                    if let Err(err) = self.accept_message() {
                        match err {
                            AppError::WouldBlock => {}
                            _ => error(&target!(), &format!("{:?}", err)),
                        }
                    }
                }
            }

            // Check if shutdown requested
            if self.visitor.lock().unwrap().get_shutdown_requested() {
                self.polling = false;
                self.closing = true;
            }
        }

        if polling_error.is_some() {
            error(&target!(), &format!("{:?}", &polling_error));
        }

        info(
            &target!(),
            &format!(
                "Polling messages ended: server_addr={:?}",
                &self.server_addr
            ),
        );

        if self.closing {
            self.perform_shutdown();
        }

        Ok(())
    }

    /// New client message acceptance processor
    fn accept_message(&mut self) -> Result<(), AppError> {
        // Accept message
        let mut buffer = [0; RECV_BUFFER_SIZE];

        let (message_size, peer_addr) = self
            .server_socket
            .as_ref()
            .unwrap()
            .recv_from(&mut buffer)
            .map_err(|err| {
                if err.kind() == io::ErrorKind::WouldBlock {
                    AppError::WouldBlock
                } else {
                    AppError::GenWithMsgAndErr(
                        format!(
                            "Error receiving message: server_addr={:?}",
                            &self.server_addr
                        ),
                        Box::new(err),
                    )
                }
            })?;

        debug(
            &target!(),
            &format!("Client message recvd: size={}", message_size),
        );

        self.visitor.lock().unwrap().on_message_received(
            &self.server_socket.as_ref().unwrap().local_addr().unwrap(),
            &peer_addr,
            buffer[..message_size].to_vec(),
        )
    }

    fn assert_listening(&self) -> Result<(), AppError> {
        if self.server_socket.is_none() {
            return Err(AppError::General("Gateway not listening".to_string()));
        }
        Ok(())
    }
}

unsafe impl Send for Server {}

/// Visitor pattern used to customize server implementation strategy.
pub trait ServerVisitor: Send {
    /// Server listener bound
    fn on_listening(&mut self) -> Result<(), AppError> {
        Ok(())
    }

    /// Client message received
    fn on_message_received(
        &mut self,
        local_addr: &SocketAddr,
        peer_addr: &SocketAddr,
        data: Vec<u8>,
    ) -> Result<(), AppError>;

    /// Returns whether listener shutdown is required
    fn get_shutdown_requested(&self) -> bool;
}
