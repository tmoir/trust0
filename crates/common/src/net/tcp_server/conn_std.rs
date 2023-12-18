use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;
use std::{io, thread};

use anyhow::Result;

use crate::error::AppError;
use crate::logging::error;
use crate::net::stream_utils;
use crate::target;

const READ_BLOCK_SIZE: usize = 1024;

/// Connection event message channel
#[derive(Debug)]
pub enum ConnectionEvent {
    Closing,
    Closed,
    Write(Vec<u8>),
}

impl ConnectionEvent {
    /// Create multiple producer, single consumer message channel
    pub fn create_channel() -> (Sender<ConnectionEvent>, Receiver<ConnectionEvent>) {
        mpsc::channel()
    }
}

/// This is a TCP client connection which has been accepted by the server, and is currently being served.
pub struct Connection {
    visitor: Box<dyn ConnectionVisitor>,
    tcp_stream: Option<TcpStream>,
    stream_reader: Box<dyn Read + Send>,
    stream_writer: Box<dyn Write + Send>,
    event_channel: (Sender<ConnectionEvent>, Receiver<ConnectionEvent>),
    closed: bool,
}

impl Connection {
    /// Connection constructor
    pub fn new(
        mut visitor: Box<dyn ConnectionVisitor>,
        tcp_stream: TcpStream,
    ) -> Result<Self, AppError> {
        let event_channel = ConnectionEvent::create_channel();
        visitor.set_event_channel_sender(event_channel.0.clone())?;
        visitor.on_connected()?;

        let stream_reader = Box::new(stream_utils::clone_std_tcp_stream(&tcp_stream)?);
        let stream_writer = Box::new(stream_utils::clone_std_tcp_stream(&tcp_stream)?);

        Ok(Self {
            visitor,
            tcp_stream: Some(tcp_stream),
            stream_reader,
            stream_writer,
            event_channel,
            closed: false,
        })
    }

    /// Connection 'closed' state accessor
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Connection 'closed' state mutator
    pub fn set_closed(&mut self, closed: bool) {
        self.closed = closed;
    }

    /// Connection 'tcp_stream' (immutable) accessor
    pub fn get_tcp_stream_as_ref(&self) -> &TcpStream {
        self.tcp_stream.as_ref().unwrap()
    }

    /// Connection 'tcp_stream' (mutable) accessor
    pub fn get_tcp_stream_as_mut(&mut self) -> &mut TcpStream {
        self.tcp_stream.as_mut().unwrap()
    }

    /// Get copy of event channel sender
    pub fn clone_event_channel_sender(&self) -> Sender<ConnectionEvent> {
        self.event_channel.0.clone()
    }

    /// Poll connection events loop
    pub fn poll_connection(&mut self) -> Result<(), AppError> {
        loop {
            // Read connection data (if avail)
            if let Err(err) = self.read() {
                error(&target!(), &format!("{:?}", err));
            }

            // Custom polling cycle handler
            if let Err(err) = self.visitor.on_polling_cycle() {
                error(&target!(), &format!("{:?}", err));
            }

            // Poll connection event
            'EVENTS: loop {
                match self.event_channel.1.try_recv() {
                    // Handle write request
                    Ok(ConnectionEvent::Write(data)) => {
                        if let Err(err) = self.write(&data) {
                            error(&target!(), &format!("{:?}", err));
                        }
                    }

                    // Handle connection shutdown request
                    Ok(ConnectionEvent::Closing) => {
                        if let Err(err) = self.shutdown() {
                            error(&target!(), &format!("{:?}", err));
                        }
                    }

                    Ok(ConnectionEvent::Closed) => break,

                    // No event
                    Err(TryRecvError::Empty) => break,

                    // Channel closed
                    Err(TryRecvError::Disconnected) => break 'EVENTS,
                }

                thread::sleep(Duration::from_millis(10));
            }

            if self.closed {
                break;
            }

            // End of poll cycle
            thread::sleep(Duration::from_millis(50));
        }

        Ok(())
    }

    /// Read and process client connection content
    pub fn read(&mut self) -> Result<Vec<u8>, AppError> {
        let mut return_buffer = vec![];
        let mut error: Option<AppError> = None;

        // Attempt connection read
        match self.read_tcp_stream() {
            Ok(buffer) => {
                if !buffer.is_empty() {
                    match self.visitor.on_connection_read(&buffer) {
                        Ok(()) => {}
                        Err(err) => error = Some(err),
                    }
                    return_buffer = buffer;
                }
            }

            Err(err) => error = Some(err),
        }

        // Handle connection error
        if error.is_some() {
            self.event_channel
                .0
                .send(ConnectionEvent::Closing)
                .map_err(|err| {
                    AppError::GenWithMsgAndErr(
                        "Error sending closing event".to_string(),
                        Box::new(err),
                    )
                })?;
            return Err(error.unwrap());
        }

        Ok(return_buffer)
    }

    /// Write content to client connection
    pub fn write(&mut self, buffer: &[u8]) -> Result<(), AppError> {
        let mut error: Option<AppError> = None;

        // Attempt connection write
        match self.write_tcp_stream(buffer) {
            Ok(()) => {}
            Err(err) => error = Some(err),
        }

        // Handle connection error
        if error.is_some() {
            self.event_channel
                .0
                .send(ConnectionEvent::Closing)
                .map_err(|err| {
                    AppError::GenWithMsgAndErr(
                        "Error sending closing event".to_string(),
                        Box::new(err),
                    )
                })?;
            return Err(error.unwrap());
        }

        Ok(())
    }

    /// Shut down TCP connection
    pub fn shutdown(&mut self) -> Result<(), AppError> {
        if self.closed {
            return Ok(());
        }

        self.tcp_stream
            .as_ref()
            .unwrap()
            .shutdown(Shutdown::Both)
            .map_err(|err| {
                AppError::GenWithMsgAndErr(
                    "Error shutting down TCP connection".to_string(),
                    Box::new(err),
                )
            })?;

        self.closed = true;

        if let Err(err) = self
            .event_channel
            .0
            .send(ConnectionEvent::Closed)
            .map_err(|err| {
                AppError::GenWithMsgAndErr("Error sending closed event".to_string(), Box::new(err))
            })
        {
            error(&target!(), &format!("{:?}", err));
        }

        self.visitor.on_shutdown()
    }

    /// Read client connection content
    fn read_tcp_stream(&mut self) -> Result<Vec<u8>, AppError> {
        let mut buffer = Vec::new();
        let mut buff_chunk = [0; READ_BLOCK_SIZE];
        loop {
            let bytes_read = match self.stream_reader.read(&mut buff_chunk) {
                Ok(bytes_read) => bytes_read,

                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                    self.event_channel
                        .0
                        .send(ConnectionEvent::Closing)
                        .map_err(|err| {
                            AppError::GenWithMsgAndErr(
                                "Error sending closing event".to_string(),
                                Box::new(err),
                            )
                        })?;
                    break;
                }

                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,

                Err(err) => {
                    return Err(AppError::GenWithMsgAndErr(
                        "Error reading from TCP connection".to_string(),
                        Box::new(err),
                    ))
                }
            };
            if bytes_read < READ_BLOCK_SIZE {
                buffer.append(&mut buff_chunk[..bytes_read].to_vec());
                break;
            }
            buffer.append(&mut buff_chunk.to_vec());
        }

        Ok(buffer)
    }

    /// Write content to client connection
    fn write_tcp_stream(&mut self, buffer: &[u8]) -> Result<(), AppError> {
        match self.stream_writer.write_all(buffer) {
            Ok(()) => {}

            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => self
                .event_channel
                .0
                .send(ConnectionEvent::Closing)
                .map_err(|err| {
                    AppError::GenWithMsgAndErr(
                        "Error sending closing event".to_string(),
                        Box::new(err),
                    )
                })?,

            Err(err) if err.kind() == io::ErrorKind::WouldBlock => self
                .event_channel
                .0
                .send(ConnectionEvent::Write(buffer.to_vec()))
                .map_err(|err| {
                    AppError::GenWithMsgAndErr(
                        "Error sending write event".to_string(),
                        Box::new(err),
                    )
                })?,

            Err(err) => {
                return Err(AppError::GenWithMsgAndErr(
                    "Error writing to TCP connection".to_string(),
                    Box::new(err),
                ))
            }
        }

        Ok(())
    }
}

unsafe impl Send for Connection {}

impl From<Connection> for TcpStream {
    fn from(value: Connection) -> Self {
        value.tcp_stream.unwrap()
    }
}

/// Visitor pattern used to customize connection implementation strategy.
pub trait ConnectionVisitor: Send {
    /// Session connected
    fn on_connected(&mut self) -> Result<(), AppError> {
        Ok(())
    }

    /// Setup event channel sender
    fn set_event_channel_sender(
        &mut self,
        _event_channel_sender: Sender<ConnectionEvent>,
    ) -> Result<(), AppError> {
        Ok(())
    }

    /// Incoming connection content processing event handler
    fn on_connection_read(&mut self, _data: &[u8]) -> Result<(), AppError> {
        Ok(())
    }

    /// Polling cycle tick handler
    fn on_polling_cycle(&mut self) -> Result<(), AppError> {
        Ok(())
    }

    /// Connection shutdown event handler
    fn on_shutdown(&mut self) -> Result<(), AppError> {
        Ok(())
    }

    /// Send error response message to client
    fn send_error_response(&mut self, err: &AppError);
}

/// Unit tests
#[cfg(test)]
pub mod tests {
    use super::*;
    use mockall::{mock, predicate};
    use std::io::ErrorKind;

    // mocks
    // =====

    mock! {
        pub ConnVisit {}
        impl ConnectionVisitor for ConnVisit {
            fn on_connected(&mut self) -> Result<(), AppError>;
            fn set_event_channel_sender(&mut self, event_channel_sender: Sender<ConnectionEvent>) -> Result<(), AppError>;
            fn on_connection_read(&mut self, data: &[u8]) -> Result<(), AppError>;
            fn on_polling_cycle(&mut self) -> Result<(), AppError>;
            fn on_shutdown(&mut self) -> Result<(), AppError>;
            fn send_error_response(&mut self, err: &AppError);
        }
    }

    // tests
    // =====

    #[test]
    fn conn_read_when_no_data_to_read() {
        let conn_visitor = MockConnVisit::new();
        let stream_writer = stream_utils::tests::MockStreamWriter::new();
        let event_channel = mpsc::channel();

        let mut stream_reader = stream_utils::tests::MockStreamReader::new();
        let buffer = [0; READ_BLOCK_SIZE];
        stream_reader
            .expect_read()
            .with(predicate::eq(buffer))
            .times(1)
            .return_once(|_| {
                Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    AppError::General("not readable".to_string()),
                ))
            });

        let mut conn = Connection {
            visitor: Box::new(conn_visitor),
            tcp_stream: None,
            stream_reader: Box::new(stream_reader),
            stream_writer: Box::new(stream_writer),
            event_channel,
            closed: false,
        };

        let result = conn.read();

        if let Err(err) = result {
            panic!("Unexpected result: err={:?}", &err);
        }

        assert!(result.unwrap().is_empty());

        match conn.event_channel.1.try_recv() {
            Ok(event) => panic!("Unexpected conn event recvd: evt={:?}", event),
            Err(err) => {
                if let TryRecvError::Empty = err {
                } else {
                    panic!("Unexpected conn event channel result: err={:?}", &err);
                }
            }
        }
    }

    #[test]
    fn conn_read_when_data_to_read() {
        let stream_writer = stream_utils::tests::MockStreamWriter::new();
        let event_channel = mpsc::channel();

        let readable_bytes = "hello".as_bytes().to_vec();

        let mut stream_reader = stream_utils::tests::MockStreamReader::new();
        let readable_bytes_copy = readable_bytes.clone();
        let buffer = [0; READ_BLOCK_SIZE];
        stream_reader
            .expect_read()
            .with(predicate::eq(buffer))
            .times(1)
            .return_once(move |b| {
                for i in 0..readable_bytes_copy.len() {
                    b[i] = *readable_bytes_copy.get(i).unwrap();
                }
                Ok(readable_bytes_copy.len())
            });

        let readable_bytes_copy = readable_bytes.clone();
        let mut conn_visitor = MockConnVisit::new();
        conn_visitor
            .expect_on_connection_read()
            .with(predicate::eq(readable_bytes_copy))
            .times(1)
            .return_once(|_| Ok(()));

        let mut conn = Connection {
            visitor: Box::new(conn_visitor),
            tcp_stream: None,
            stream_reader: Box::new(stream_reader),
            stream_writer: Box::new(stream_writer),
            event_channel,
            closed: false,
        };

        let result = conn.read();

        if let Err(err) = result {
            panic!("Unexpected result: err={:?}", &err);
        }

        let recvd_bytes = result.unwrap();
        assert_eq!(recvd_bytes.len(), readable_bytes.len());
        assert_eq!(
            String::from_utf8(recvd_bytes.clone()).unwrap(),
            String::from_utf8(readable_bytes.clone()).unwrap()
        );

        match conn.event_channel.1.try_recv() {
            Ok(event) => panic!("Unexpected conn event recvd: evt={:?}", event),
            Err(err) => {
                if let TryRecvError::Empty = err {
                } else {
                    panic!("Unexpected conn event channel result: err={:?}", &err);
                }
            }
        }
    }

    #[test]
    fn conn_read_when_peer_connection_closed() {
        let stream_writer = stream_utils::tests::MockStreamWriter::new();
        let event_channel = mpsc::channel();

        let mut stream_reader = stream_utils::tests::MockStreamReader::new();
        let buffer = [0; READ_BLOCK_SIZE];
        stream_reader
            .expect_read()
            .with(predicate::eq(buffer))
            .times(1)
            .return_once(|_| {
                Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    AppError::General("connection closed".to_string()),
                ))
            });

        let mut conn_visitor = MockConnVisit::new();
        conn_visitor.expect_on_connection_read().never();

        let mut conn = Connection {
            visitor: Box::new(conn_visitor),
            tcp_stream: None,
            stream_reader: Box::new(stream_reader),
            stream_writer: Box::new(stream_writer),
            event_channel,
            closed: false,
        };

        let result = conn.read();

        if let Err(err) = result {
            panic!("Unexpected result: err={:?}", &err);
        }

        let recvd_bytes = result.unwrap();
        assert_eq!(recvd_bytes.len(), 0);

        match conn.event_channel.1.try_recv() {
            Ok(event) => {
                if let ConnectionEvent::Closing = event {
                } else {
                    panic!("Unexpected conn event recvd: evt={:?}", event)
                }
            }
            Err(err) => {
                panic!("Unexpected conn event channel result: err={:?}", &err);
            }
        }
    }

    #[test]
    fn conn_read_when_error_while_reading() {
        let stream_writer = stream_utils::tests::MockStreamWriter::new();
        let event_channel = mpsc::channel();

        let mut stream_reader = stream_utils::tests::MockStreamReader::new();
        let buffer = [0; READ_BLOCK_SIZE];
        stream_reader
            .expect_read()
            .with(predicate::eq(buffer))
            .times(1)
            .return_once(|_| {
                Err(io::Error::new(
                    ErrorKind::Other,
                    AppError::General("error1".to_string()),
                ))
            });

        let mut conn_visitor = MockConnVisit::new();
        conn_visitor.expect_on_connection_read().never();

        let mut conn = Connection {
            visitor: Box::new(conn_visitor),
            tcp_stream: None,
            stream_reader: Box::new(stream_reader),
            stream_writer: Box::new(stream_writer),
            event_channel,
            closed: false,
        };

        let result = conn.read();

        if let Ok(recvd_bytes) = result {
            panic!("Unexpected successful result: buffer={:?}", &recvd_bytes);
        }

        match conn.event_channel.1.try_recv() {
            Ok(event) => {
                if let ConnectionEvent::Closing = event {
                } else {
                    panic!("Unexpected conn event recvd: evt={:?}", event)
                }
            }
            Err(err) => {
                panic!("Unexpected conn event channel result: err={:?}", &err);
            }
        }
    }

    #[test]
    fn conn_write_when_stream_not_writable() {
        let event_channel = mpsc::channel();

        let mut stream_writer = stream_utils::tests::MockStreamWriter::new();
        let buffer = "hello".as_bytes();
        stream_writer
            .expect_write_all()
            .with(predicate::eq(buffer))
            .times(1)
            .return_once(|_| {
                Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    AppError::General("not writable".to_string()),
                ))
            });

        let mut conn = Connection {
            visitor: Box::new(MockConnVisit::new()),
            tcp_stream: None,
            stream_reader: Box::new(stream_utils::tests::MockStreamReader::new()),
            stream_writer: Box::new(stream_writer),
            event_channel,
            closed: false,
        };

        let result = conn.write(buffer);

        if let Err(err) = result {
            panic!("Unexpected result: err={:?}", &err);
        }

        match conn.event_channel.1.try_recv() {
            Ok(event) => {
                if let ConnectionEvent::Write(_) = event {
                } else {
                    panic!("Unexpected conn event recvd: evt={:?}", event)
                }
            }
            Err(err) => panic!("Unexpected conn event channel result: err={:?}", &err),
        }
    }

    #[test]
    fn conn_write_when_successfully_written() {
        let event_channel = mpsc::channel();

        let mut stream_writer = stream_utils::tests::MockStreamWriter::new();
        let buffer = "hello".as_bytes();
        stream_writer
            .expect_write_all()
            .with(predicate::eq(buffer))
            .times(1)
            .return_once(|_| Ok(()));

        let mut conn = Connection {
            visitor: Box::new(MockConnVisit::new()),
            tcp_stream: None,
            stream_reader: Box::new(stream_utils::tests::MockStreamReader::new()),
            stream_writer: Box::new(stream_writer),
            event_channel,
            closed: false,
        };

        let result = conn.write(buffer);

        if let Err(err) = result {
            panic!("Unexpected result: err={:?}", &err);
        }

        match conn.event_channel.1.try_recv() {
            Ok(event) => panic!("Unexpected conn event recvd: evt={:?}", event),
            Err(err) => {
                if let TryRecvError::Empty = err {
                } else {
                    panic!("Unexpected conn event channel result: err={:?}", &err);
                }
            }
        }
    }

    #[test]
    fn conn_write_when_peer_connection_closed() {
        let event_channel = mpsc::channel();

        let mut stream_writer = stream_utils::tests::MockStreamWriter::new();
        let buffer = "hello".as_bytes();
        stream_writer
            .expect_write_all()
            .with(predicate::eq(buffer))
            .times(1)
            .return_once(|_| {
                Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    AppError::General("connection closed".to_string()),
                ))
            });

        let mut conn = Connection {
            visitor: Box::new(MockConnVisit::new()),
            tcp_stream: None,
            stream_reader: Box::new(stream_utils::tests::MockStreamReader::new()),
            stream_writer: Box::new(stream_writer),
            event_channel,
            closed: false,
        };

        let result = conn.write(buffer);

        if let Err(err) = result {
            panic!("Unexpected result: err={:?}", &err);
        }

        match conn.event_channel.1.try_recv() {
            Ok(event) => {
                if let ConnectionEvent::Closing = event {
                } else {
                    panic!("Unexpected conn event recvd: evt={:?}", event)
                }
            }
            Err(err) => {
                panic!("Unexpected conn event channel result: err={:?}", &err);
            }
        }
    }

    #[test]
    fn conn_write_when_error_while_reading() {
        let event_channel = mpsc::channel();

        let mut stream_writer = stream_utils::tests::MockStreamWriter::new();
        let buffer = "hello".as_bytes();
        stream_writer
            .expect_write_all()
            .with(predicate::eq(buffer))
            .times(1)
            .return_once(|_| {
                Err(io::Error::new(
                    ErrorKind::Other,
                    AppError::General("error1".to_string()),
                ))
            });

        let mut conn = Connection {
            visitor: Box::new(MockConnVisit::new()),
            tcp_stream: None,
            stream_reader: Box::new(stream_utils::tests::MockStreamReader::new()),
            stream_writer: Box::new(stream_writer),
            event_channel,
            closed: false,
        };

        let result = conn.write(buffer);

        if let Ok(()) = result {
            panic!("Unexpected successful result");
        }

        match conn.event_channel.1.try_recv() {
            Ok(event) => {
                if let ConnectionEvent::Closing = event {
                } else {
                    panic!("Unexpected conn event recvd: evt={:?}", event)
                }
            }
            Err(err) => {
                panic!("Unexpected conn event channel result: err={:?}", &err);
            }
        }
    }
}
