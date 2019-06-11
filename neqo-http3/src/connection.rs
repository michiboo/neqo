// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::hframe::{HFrame, HFrameReader, HSettingType};
use crate::request_stream_client::RequestStreamClient;
use crate::request_stream_server::RequestStreamServer;
use neqo_common::{
    qdebug, qerror, qinfo, qwarn, Decoder, Encoder, IncrementalDecoder, IncrementalDecoderResult,
};
use neqo_qpack::decoder::{QPackDecoder, QPACK_UNI_STREAM_TYPE_DECODER};
use neqo_qpack::encoder::{QPackEncoder, QPACK_UNI_STREAM_TYPE_ENCODER};
use neqo_transport::connection::Role;

use neqo_transport::connection::Connection;
use neqo_transport::frame::StreamType;
use neqo_transport::{AppError, ConnectionEvent, Datagram, State};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::mem;
use std::rc::Rc;

use crate::{Error, Http3Error, Res};

const HTTP3_UNI_STREAM_TYPE_CONTROL: u64 = 0x0;
const HTTP3_UNI_STREAM_TYPE_PUSH: u64 = 0x1;

const MAX_HEADER_LIST_SIZE_DEFAULT: u64 = u64::max_value();
const NUM_PLACEHOLDERS_DEFAULT: u64 = 0;

// The local control stream, responsible for encoding frames and sending them
#[derive(Default, Debug)]
struct ControlStreamLocal {
    stream_id: Option<u64>,
    buf: Vec<u8>,
}

impl ControlStreamLocal {
    pub fn send_frame(&mut self, f: HFrame) {
        let mut enc = Encoder::default();
        f.encode(&mut enc);
        self.buf.append(&mut enc.into());
    }
    pub fn send(&mut self, conn: &mut Connection) -> Res<()> {
        if let Some(stream_id) = self.stream_id {
            if !self.buf.is_empty() {
                let sent = conn.stream_send(stream_id, &self.buf[..])?;
                if sent == self.buf.len() {
                    self.buf.clear();
                } else {
                    let b = self.buf.split_off(sent);
                    self.buf = b;
                }
            }
            return Ok(());
        }
        Ok(())
    }
}

// The remote control stream is responsible only for reading frames. The frames are handled by Http3Connection
#[derive(Debug)]
struct ControlStreamRemote {
    stream_id: Option<u64>,
    frame_reader: HFrameReader,
    fin: bool,
}

impl ::std::fmt::Display for ControlStreamRemote {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "Http3 remote control stream {:?}", self.stream_id)
    }
}

impl ControlStreamRemote {
    pub fn new() -> ControlStreamRemote {
        ControlStreamRemote {
            stream_id: None,
            frame_reader: HFrameReader::new(),
            fin: false,
        }
    }

    pub fn add_remote_stream(&mut self, stream_id: u64) -> Res<()> {
        qinfo!([self] "A new control stream {}.", stream_id);
        if self.stream_id.is_some() {
            qdebug!([self] "A control stream already exists");
            return Err(Error::WrongStreamCount);
        }
        self.stream_id = Some(stream_id);
        Ok(())
    }

    pub fn receive_if_this_stream(&mut self, conn: &mut Connection, stream_id: u64) -> Res<bool> {
        if let Some(id) = self.stream_id {
            if id == stream_id {
                self.fin = self.frame_reader.receive(conn, stream_id)?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[derive(Debug)]
struct NewStreamTypeReader {
    reader: IncrementalDecoder,
    fin: bool,
}

impl NewStreamTypeReader {
    pub fn new() -> NewStreamTypeReader {
        NewStreamTypeReader {
            reader: IncrementalDecoder::decode_varint(),
            fin: false,
        }
    }
    pub fn get_type(&mut self, conn: &mut Connection, stream_id: u64) -> Option<u64> {
        // On any error we will only close this stream!
        loop {
            let to_read = self.reader.min_remaining();
            let mut buf = vec![0; to_read];
            match conn.stream_recv(stream_id, &mut buf[..]) {
                Ok((_, true)) => {
                    self.fin = true;
                    break None;
                }
                Ok((0, false)) => {
                    break None;
                }
                Ok((amount, false)) => {
                    let mut dec = Decoder::from(&buf[..amount]);
                    match self.reader.consume(&mut dec) {
                        IncrementalDecoderResult::Uint(v) => {
                            break Some(v);
                        }
                        IncrementalDecoderResult::InProgress => {}
                        _ => {
                            break None;
                        }
                    }
                }
                Err(e) => {
                    qdebug!([conn] "Error reading stream type for stream {}: {:?}", stream_id, e);
                    self.fin = true;
                    break None;
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum Http3State {
    Initializing,
    Connected,
    GoingAway,
    Closing(AppError),
    Closed(AppError),
}

pub struct Http3Connection {
    state: Http3State,
    conn: Connection,
    max_header_list_size: u64,
    num_placeholders: u64,
    control_stream_local: ControlStreamLocal,
    control_stream_remote: ControlStreamRemote,
    new_streams: HashMap<u64, NewStreamTypeReader>,
    qpack_encoder: QPackEncoder,
    qpack_decoder: QPackDecoder,
    settings_received: bool,
    streams_are_readable: BTreeSet<u64>,
    streams_have_data_to_send: BTreeSet<u64>,
    // Client only
    events: Rc<RefCell<Http3Events>>,
    request_streams_client: HashMap<u64, RequestStreamClient>,
    // Server only
    handler: Option<Box<FnMut(&[(String, String)], bool) -> (Vec<(String, String)>, Vec<u8>)>>,
    request_streams_server: HashMap<u64, RequestStreamServer>,
}

impl ::std::fmt::Display for Http3Connection {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "Http3 connection {:?}", self.role())
    }
}

impl Http3Connection {
    pub fn new(
        c: Connection,
        max_table_size: u32,
        max_blocked_streams: u16,
        handler: Option<Box<FnMut(&[(String, String)], bool) -> (Vec<(String, String)>, Vec<u8>)>>,
    ) -> Http3Connection {
        qinfo!(
            "Create new http connection with max_table_size: {} and max_blocked_streams: {}",
            max_table_size,
            max_blocked_streams
        );
        if max_table_size > (1 << 30) - 1 {
            panic!("Wrong max_table_size");
        }
        Http3Connection {
            state: Http3State::Initializing,
            conn: c,
            max_header_list_size: MAX_HEADER_LIST_SIZE_DEFAULT,
            num_placeholders: NUM_PLACEHOLDERS_DEFAULT,
            control_stream_local: ControlStreamLocal::default(),
            control_stream_remote: ControlStreamRemote::new(),
            qpack_encoder: QPackEncoder::new(true),
            qpack_decoder: QPackDecoder::new(max_table_size, max_blocked_streams),
            new_streams: HashMap::new(),
            request_streams_client: HashMap::new(),
            request_streams_server: HashMap::new(),
            settings_received: false,
            streams_are_readable: BTreeSet::new(),
            streams_have_data_to_send: BTreeSet::new(),
            events: Rc::new(RefCell::new(Http3Events::default())),
            handler,
        }
    }

    fn initialize_http3_connection(&mut self) -> Res<()> {
        qdebug!([self] "initialize_http3_connection");
        self.create_control_stream()?;
        self.create_settings();
        self.create_qpack_streams()?;
        Ok(())
    }

    fn create_control_stream(&mut self) -> Res<()> {
        qdebug!([self] "create_control_stream.");
        self.control_stream_local.stream_id = Some(self.conn.stream_create(StreamType::UniDi)?);
        let mut enc = Encoder::default();
        enc.encode_varint(HTTP3_UNI_STREAM_TYPE_CONTROL as u64);
        self.control_stream_local.buf.append(&mut enc.into());
        Ok(())
    }

    fn create_qpack_streams(&mut self) -> Res<()> {
        qdebug!([self] "create_qpack_streams.");
        self.qpack_encoder
            .add_send_stream(self.conn.stream_create(StreamType::UniDi)?);
        self.qpack_decoder
            .add_send_stream(self.conn.stream_create(StreamType::UniDi)?);
        Ok(())
    }

    fn create_settings(&mut self) {
        qdebug!([self] "create_settings.");
        self.control_stream_local.send_frame(HFrame::Settings {
            settings: vec![
                (
                    HSettingType::MaxTableSize,
                    self.qpack_decoder.get_max_table_size().into(),
                ),
                (
                    HSettingType::BlockedStreams,
                    self.qpack_decoder.get_blocked_streams().into(),
                ),
            ],
        });
    }

    // This function takes the provided result and check for an error.
    // An error results in closing the connection.
    fn check_result<T>(&mut self, res: Res<T>) -> bool {
        match &res {
            Err(e) => {
                qinfo!([self] "Connection error: {}.", e);
                self.close(e.code(), format!("{}", e));
                true
            }
            _ => false,
        }
    }

    fn role(&self) -> Role {
        self.conn.role()
    }

    pub fn check_state_change(&mut self) {
        match self.state {
            Http3State::Initializing => {
                if self.conn.state().clone() == State::Connected {
                    self.state = Http3State::Connected;
                    let res = self.initialize_http3_connection();
                    self.check_result(res);
                }
            }
            Http3State::Closing(err) => {
                if let State::Closed(..) = *self.conn.state() {
                    self.state = Http3State::Closed(err);
                }
            }
            _ => {}
        }
    }

    pub fn process<I>(&mut self, in_dgrams: I, cur_time: u64) -> (Vec<Datagram>, u64)
    where
        I: IntoIterator<Item = Datagram>,
    {
        qdebug!([self] "Process.");
        self.process_input(in_dgrams, cur_time);
        self.process_http3();
        self.process_output(cur_time)
    }

    pub fn process_input<I>(&mut self, in_dgrams: I, cur_time: u64)
    where
        I: IntoIterator<Item = Datagram>,
    {
        qdebug!([self] "Process input.");
        self.conn.process_input(in_dgrams, cur_time);
        self.check_state_change();
    }

    pub fn conn(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn process_http3(&mut self) {
        qdebug!([self] "Process http3 internal.");
        match self.state {
            Http3State::Connected | Http3State::GoingAway => {
                let res = self.check_connection_events();
                if self.check_result(res) {
                    return;
                }
                let res = self.process_reading();
                if self.check_result(res) {
                    return;
                }
                let res = self.process_sending();
                self.check_result(res);
            }
            _ => {}
        }
    }

    pub fn process_output(&mut self, cur_time: u64) -> (Vec<Datagram>, u64) {
        qdebug!([self] "Process output.");
        self.conn.process_output(cur_time)
    }

    // If this return an error the connection must be closed.
    fn process_reading(&mut self) -> Res<()> {
        let readable = mem::replace(&mut self.streams_are_readable, BTreeSet::new());
        for stream_id in readable.iter() {
            self.handle_stream_readable(*stream_id)?;
        }
        Ok(())
    }

    fn process_sending(&mut self) -> Res<()> {
        // check if control stream has data to send.
        self.control_stream_local.send(&mut self.conn)?;

        let to_send = mem::replace(&mut self.streams_have_data_to_send, BTreeSet::new());
        if self.role() == Role::Client {
            for stream_id in to_send {
                if let Some(cs) = &mut self.request_streams_client.get_mut(&stream_id) {
                    cs.send(&mut self.conn, &mut self.qpack_encoder)?;
                    if cs.has_data_to_send() {
                        self.streams_have_data_to_send.insert(stream_id);
                    }
                }
            }
        } else {
            for stream_id in to_send {
                let mut remove_stream = false;
                if let Some(cs) = &mut self.request_streams_server.get_mut(&stream_id) {
                    cs.send(&mut self.conn)?;
                    if cs.has_data_to_send() {
                        self.streams_have_data_to_send.insert(stream_id);
                    } else {
                        remove_stream = true;
                    }
                }
                if remove_stream {
                    self.request_streams_server.remove(&stream_id);
                }
            }
        }
        self.qpack_decoder.send(&mut self.conn)?;
        self.qpack_encoder.send(&mut self.conn)?;
        Ok(())
    }

    // If this return an error the connection must be closed.
    fn check_connection_events(&mut self) -> Res<()> {
        qdebug!([self] "check_connection_events");
        let events = self.conn.events();
        for e in events {
            qdebug!([self] "check_connection_events - event {:?}.", e);
            match e {
                ConnectionEvent::NewStream {
                    stream_id,
                    stream_type,
                } => self.handle_new_stream(stream_id, stream_type)?,
                ConnectionEvent::SendStreamWritable { .. } => {}
                ConnectionEvent::RecvStreamReadable { stream_id } => {
                    self.streams_are_readable.insert(stream_id);
                }
                ConnectionEvent::RecvStreamReset {
                    stream_id,
                    app_error,
                } => self.handle_stream_reset(stream_id, app_error)?,
                ConnectionEvent::SendStreamStopSending {
                    stream_id,
                    app_error,
                } => self.handle_stream_stop_sending(stream_id, app_error)?,
                ConnectionEvent::SendStreamComplete { stream_id } => {
                    self.handle_stream_complete(stream_id)?
                }
                ConnectionEvent::SendStreamCreatable { stream_type } => {
                    self.handle_stream_creatable(stream_type)?
                }
                ConnectionEvent::ConnectionClosed { error_code, .. } => {
                    self.handle_connection_closed(error_code)?
                }
                ConnectionEvent::ZeroRttRejected => {
                    // TODO(mt) work out what to do here.
                    // Everything will have to be redone: SETTINGS, qpack streams, and requests.
                }
            }
        }
        Ok(())
    }

    fn handle_new_stream(&mut self, stream_id: u64, stream_type: StreamType) -> Res<()> {
        qdebug!([self] "A new stream: {:?} {}.", stream_type, stream_id);
        match stream_type {
            StreamType::BiDi => match self.role() {
                Role::Server => self.handle_new_client_request(stream_id),
                Role::Client => {
                    qerror!("Client received a new bidirectional stream!");
                    // TODO: passing app error of 0, check if there's a better value
                    self.conn.stream_stop_sending(stream_id, 0)?;
                }
            },
            StreamType::UniDi => {
                let stream_type;
                let fin;
                {
                    let ns = &mut self
                        .new_streams
                        .entry(stream_id)
                        .or_insert_with(NewStreamTypeReader::new);
                    stream_type = ns.get_type(&mut self.conn, stream_id);
                    fin = ns.fin;
                }

                if fin {
                    self.new_streams.remove(&stream_id);
                } else if let Some(t) = stream_type {
                    self.decode_new_stream(t, stream_id)?;
                    self.new_streams.remove(&stream_id);
                }
            }
        };
        Ok(())
    }

    fn handle_stream_readable(&mut self, stream_id: u64) -> Res<()> {
        qdebug!([self] "Readable stream {}.", stream_id);
        let label = if ::log::log_enabled!(::log::Level::Debug) {
            format!("{}", self)
        } else {
            String::new()
        };
        let mut unblocked_streams: Vec<u64> = Vec::new();

        if self.read_stream_client(stream_id, false)? {
            qdebug!([label] "Request/response stream {} read.", stream_id);
        } else if self.read_stream_server(stream_id, false)? {
        } else if self
            .control_stream_remote
            .receive_if_this_stream(&mut self.conn, stream_id)?
        {
            qdebug!(
                [self]
                "The remote control stream ({}) is readable.",
                stream_id
            );
            while self.control_stream_remote.frame_reader.done() || self.control_stream_remote.fin {
                self.handle_control_frame()?;
                self.control_stream_remote
                    .receive_if_this_stream(&mut self.conn, stream_id)?;
            }
        } else if self
            .qpack_encoder
            .recv_if_encoder_stream(&mut self.conn, stream_id)?
        {
            qdebug!(
                [self]
                "The qpack encoder stream ({}) is readable.",
                stream_id
            );
        } else if self.qpack_decoder.is_recv_stream(stream_id) {
            qdebug!(
                [self]
                "The qpack decoder stream ({}) is readable.",
                stream_id
            );
            unblocked_streams = self.qpack_decoder.receive(&mut self.conn, stream_id)?;
        } else if let Some(ns) = self.new_streams.get_mut(&stream_id) {
            let stream_type = ns.get_type(&mut self.conn, stream_id);
            let fin = ns.fin;
            if fin {
                self.new_streams.remove(&stream_id);
            }
            if let Some(t) = stream_type {
                self.decode_new_stream(t, stream_id)?;
                self.new_streams.remove(&stream_id);
            }
        } else {
            // For a new stream we receive NewStream event and a
            // RecvStreamReadable event.
            // In most cases we decode a new stream already on the NewStream
            // event and remove it from self.new_streams.
            // Therefore, while processing RecvStreamReadable there will be no
            // entry for the stream in self.new_streams.
            qdebug!("Unknown stream.");
        }

        for stream_id in unblocked_streams {
            qdebug!([self] "Stream {} is unblocked", stream_id);
            if self.role() == Role::Client {
                self.read_stream_client(stream_id, true)?;
            } else {
                self.read_stream_server(stream_id, true)?;
            }
        }
        Ok(())
    }

    fn handle_stream_reset(&mut self, _stream_id: u64, _app_err: AppError) -> Res<()> {
        Ok(())
    }

    fn handle_stream_stop_sending(&mut self, _stream_id: u64, _app_err: AppError) -> Res<()> {
        Ok(())
    }

    fn handle_stream_complete(&mut self, _stream_id: u64) -> Res<()> {
        Ok(())
    }

    fn handle_stream_creatable(&mut self, _stream_type: StreamType) -> Res<()> {
        Ok(())
    }

    fn handle_connection_closed(&mut self, error_code: u16) -> Res<()> {
        self.events.borrow_mut().connection_closed(error_code);
        self.state = Http3State::Closed(error_code);
        Ok(())
    }

    fn read_stream_client(&mut self, stream_id: u64, unblocked: bool) -> Res<bool> {
        if self.role() != Role::Client {
            return Ok(false);
        }
        let label = if ::log::log_enabled!(::log::Level::Debug) {
            format!("{}", self)
        } else {
            String::new()
        };

        let mut found = false;

        if let Some(request_stream) = &mut self.request_streams_client.get_mut(&stream_id) {
            qdebug!([label] "Request/response stream {} is readable.", stream_id);
            found = true;
            let res = if unblocked {
                request_stream.unblock(&mut self.qpack_decoder)
            } else {
                request_stream.receive(&mut self.conn, &mut self.qpack_decoder)
            };
            if let Err(e) = res {
                qdebug!([label] "Error {} ocurred", e);
                if e.is_stream_error() {
                    self.request_streams_client.remove(&stream_id);
                    self.conn.stream_stop_sending(stream_id, e.code())?;
                } else {
                    return Err(e);
                }
            } else if request_stream.done() {
                self.request_streams_client.remove(&stream_id);
            }
        }
        Ok(found)
    }

    fn read_stream_server(&mut self, stream_id: u64, unblocked: bool) -> Res<bool> {
        if self.role() != Role::Server {
            return Ok(false);
        }
        let label = if ::log::log_enabled!(::log::Level::Debug) {
            format!("{}", self)
        } else {
            String::new()
        };

        let mut found = false;

        if let Some(request_stream) = &mut self.request_streams_server.get_mut(&stream_id) {
            qdebug!([label] "Request/response stream {} is readable.", stream_id);
            found = true;
            let res = if unblocked {
                request_stream.unblock(&mut self.qpack_decoder)
            } else {
                request_stream.receive(&mut self.conn, &mut self.qpack_decoder)
            };
            if let Err(e) = res {
                qdebug!([label] "Error {} ocurred", e);
                if e.is_stream_error() {
                    self.request_streams_client.remove(&stream_id);
                    self.conn.stream_stop_sending(stream_id, e.code())?;
                } else {
                    return Err(e);
                }
            }
            if request_stream.done_reading_request() {
                if let Some(ref mut cb) = self.handler {
                    let (headers, data) = (cb)(request_stream.get_request_headers(), false);
                    request_stream.set_response(&headers, data, &mut self.qpack_encoder);
                }
                if request_stream.has_data_to_send() {
                    self.streams_have_data_to_send.insert(stream_id);
                } else {
                    self.request_streams_client.remove(&stream_id);
                }
            }
        }
        Ok(found)
    }

    fn decode_new_stream(&mut self, stream_type: u64, stream_id: u64) -> Res<()> {
        match stream_type {
            HTTP3_UNI_STREAM_TYPE_CONTROL => {
                self.control_stream_remote.add_remote_stream(stream_id)
            }

            HTTP3_UNI_STREAM_TYPE_PUSH => {
                qdebug!([self] "A new push stream {}.", stream_id);
                if self.role() == Role::Server {
                    qdebug!([self] "Error: server receives a push stream!");
                    self.conn
                        .stream_stop_sending(stream_id, Error::WrongStreamDirection.code())?;
                } else {
                    // TODO implement PUSH
                    qdebug!([self] "PUSH is not implemented!");
                    self.conn
                        .stream_stop_sending(stream_id, Error::PushRefused.code())?;
                }
                Ok(())
            }
            QPACK_UNI_STREAM_TYPE_ENCODER => {
                qinfo!([self] "A new remote qpack encoder stream {}", stream_id);
                if self.qpack_decoder.has_recv_stream() {
                    qdebug!([self] "A qpack encoder stream already exists");
                    return Err(Error::WrongStreamCount);
                }
                self.qpack_decoder.add_recv_stream(stream_id);
                self.streams_are_readable.insert(stream_id);
                Ok(())
            }
            QPACK_UNI_STREAM_TYPE_DECODER => {
                qinfo!([self] "A new remote qpack decoder stream {}", stream_id);
                if self.qpack_encoder.has_recv_stream() {
                    qdebug!([self] "A qpack decoder stream already exists");
                    return Err(Error::WrongStreamCount);
                }
                self.qpack_encoder.add_recv_stream(stream_id);
                self.streams_are_readable.insert(stream_id);
                Ok(())
            }
            // TODO reserved stream types
            _ => {
                self.conn
                    .stream_stop_sending(stream_id, Error::UnknownStreamType.code())?;
                Ok(())
            }
        }
    }

    pub fn close<S: Into<String>>(&mut self, error: AppError, msg: S) {
        qdebug!([self] "Closed.");
        self.state = Http3State::Closing(error);
        if (!self.request_streams_client.is_empty() || !self.request_streams_server.is_empty())
            && (error == 0)
        {
            qwarn!("close() called when streams still active");
        }
        self.request_streams_client.clear();
        self.request_streams_server.clear();
        self.conn.close(0, error, msg);
    }

    pub fn fetch(
        &mut self,
        method: &str,
        scheme: &str,
        host: &str,
        path: &str,
        headers: &[(String, String)],
    ) -> Res<u64> {
        qdebug!(
            [self]
            "Fetch method={}, scheme={}, host={}, path={}",
            method,
            scheme,
            host,
            path
        );
        let id = self.conn.stream_create(StreamType::BiDi)?;
        self.request_streams_client.insert(
            id,
            RequestStreamClient::new(id, method, scheme, host, path, headers, self.events.clone()),
        );
        self.streams_have_data_to_send.insert(id);
        Ok(id)
    }

    fn handle_control_frame(&mut self) -> Res<()> {
        if self.control_stream_remote.fin {
            return Err(Error::ClosedCriticalStream);
        }
        if self.control_stream_remote.frame_reader.done() {
            let f = self.control_stream_remote.frame_reader.get_frame()?;
            qdebug!([self] "Handle a control frame {:?}", f);
            if let HFrame::Settings { .. } = f {
                if self.settings_received {
                    qdebug!([self] "SETTINGS frame already received");
                    return Err(Error::UnexpectedFrame);
                }
                self.settings_received = true;
            } else if !self.settings_received {
                qdebug!([self] "SETTINGS frame not received");
                return Err(Error::MissingSettings);
            }
            return match f {
                HFrame::Settings { settings } => self.handle_settings(&settings),
                HFrame::Priority { .. } => Ok(()),
                HFrame::CancelPush { .. } => Ok(()),
                HFrame::Goaway { stream_id } => self.handle_goaway(stream_id),
                HFrame::MaxPushId { push_id } => self.handle_max_push_id(push_id),
                _ => Err(Error::WrongStream),
            };
        }
        Ok(())
    }

    fn handle_settings(&mut self, s: &[(HSettingType, u64)]) -> Res<()> {
        qdebug!([self] "Handle SETTINGS frame.");
        for (t, v) in s {
            qdebug!([self] " {:?} = {:?}", t, v);
            match t {
                HSettingType::MaxHeaderListSize => {
                    self.max_header_list_size = *v;
                }
                HSettingType::NumPlaceholders => {
                    if self.role() == Role::Server {
                        return Err(Error::WrongStreamDirection);
                    } else {
                        self.num_placeholders = *v;
                    }
                }
                HSettingType::MaxTableSize => self.qpack_encoder.set_max_capacity(*v)?,
                HSettingType::BlockedStreams => self.qpack_encoder.set_max_blocked_streams(*v)?,

                _ => {}
            }
        }
        Ok(())
    }

    fn handle_goaway(&mut self, goaway_stream_id: u64) -> Res<()> {
        qdebug!([self] "handle_goaway");
        if self.role() == Role::Server {
            return Err(Error::UnexpectedFrame);
        } else {
            // Issue reset events for streams >= goaway stream id
            self.request_streams_client
                .iter()
                .filter(|(id, _)| **id >= goaway_stream_id)
                .map(|(id, _)| *id)
                .for_each(|id| {
                    self.events
                        .borrow_mut()
                        .request_closed(id, Http3Error::NetReset)
                });

            // Actually remove (i.e. don't retain) these streams
            self.request_streams_client
                .retain(|id, _| *id < goaway_stream_id);

            // Remove events for any of these streams by creating a new set of
            // filtered events and then swapping with the original set.
            let updated_events = self
                .events
                .borrow()
                .events
                .iter()
                .filter(|evt| match evt {
                    Http3Event::HeaderReady { stream_id }
                    | Http3Event::DataReadable { stream_id }
                    | Http3Event::NewPushStream { stream_id } => *stream_id < goaway_stream_id,
                    Http3Event::RequestClosed { .. } | Http3Event::ConnectionClosed { .. } => true,
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            mem::replace(&mut self.events.borrow_mut().events, updated_events);

            if self.state == Http3State::Connected {
                self.state = Http3State::GoingAway;
            }
        }
        Ok(())
    }

    fn handle_max_push_id(&mut self, id: u64) -> Res<()> {
        qdebug!([self] "handle_max_push_id={}.", id);
        if self.role() == Role::Client {
            return Err(Error::UnexpectedFrame);
        } else {
            // TODO
        }
        Ok(())
    }

    pub fn state(&self) -> Http3State {
        self.state.clone()
    }

    // API
    pub fn get_headers(
        &mut self,
        stream_id: u64,
    ) -> Result<Option<Vec<(String, String)>>, Http3Error> {
        let label = if ::log::log_enabled!(::log::Level::Debug) {
            format!("{}", self)
        } else {
            String::new()
        };
        if let Some(cs) = &mut self.request_streams_client.get_mut(&stream_id) {
            qdebug!([label] "get_header from stream {}.", stream_id);
            Ok(cs.get_header())
        } else {
            Err(Http3Error::InvalidStreamId)
        }
    }

    pub fn read_data(
        &mut self,
        stream_id: u64,
        buf: &mut [u8],
    ) -> Result<(usize, bool), Http3Error> {
        let label = if ::log::log_enabled!(::log::Level::Debug) {
            format!("{}", self)
        } else {
            String::new()
        };
        if let Some(cs) = &mut self.request_streams_client.get_mut(&stream_id) {
            qdebug!([label] "read_data from stream {}.", stream_id);
            match cs.read_data(&mut self.conn, buf) {
                Ok((amount, fin)) => {
                    if fin {
                        self.request_streams_client.remove(&stream_id);
                    }
                    if amount > 0 && !fin {
                        self.streams_are_readable.insert(stream_id);
                    }
                    Ok((amount, fin))
                }
                Err(e) => {
                    self.close(e.code(), "");
                    return Err(Http3Error::ConnectionError);
                }
            }
        } else {
            return Err(Http3Error::InvalidStreamId);
        }
    }

    pub fn events(&mut self) -> Vec<Http3Event> {
        // Turn it into a vec for simplicity's sake
        self.events.borrow_mut().events().into_iter().collect()
    }

    // SERVER SIDE ONLY FUNCTIONS
    fn handle_new_client_request(&mut self, stream_id: u64) {
        self.request_streams_server
            .insert(stream_id, RequestStreamServer::new(stream_id));
    }
}

#[derive(Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
pub enum Http3Event {
    /// Space available in the buffer for an application write to succeed.
    HeaderReady { stream_id: u64 },
    /// New bytes available for reading.
    DataReadable { stream_id: u64 },
    /// Peer reset the stream.
    RequestClosed { stream_id: u64, error: Http3Error },
    /// A new push stream
    NewPushStream { stream_id: u64 },
    /// Peer closed the connection
    ConnectionClosed { error_code: u16 },
}

#[derive(Debug, Default)]
pub struct Http3Events {
    events: BTreeSet<Http3Event>,
}

impl Http3Events {
    pub fn header_ready(&mut self, stream_id: u64) {
        self.events.insert(Http3Event::HeaderReady { stream_id });
    }

    pub fn data_readable(&mut self, stream_id: u64) {
        self.events.insert(Http3Event::DataReadable { stream_id });
    }

    pub fn request_closed(&mut self, stream_id: u64, error: Http3Error) {
        self.events
            .insert(Http3Event::RequestClosed { stream_id, error });
    }

    pub fn new_push_stream(&mut self, stream_id: u64) {
        self.events.insert(Http3Event::NewPushStream { stream_id });
    }

    pub fn connection_closed(&mut self, error_code: u16) {
        self.events.clear();
        self.events
            .insert(Http3Event::ConnectionClosed { error_code });
    }

    pub fn events(&mut self) -> BTreeSet<Http3Event> {
        mem::replace(&mut self.events, BTreeSet::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neqo_crypto::init_db;
    use std::net::SocketAddr;

    fn loopback() -> SocketAddr {
        "127.0.0.1:443".parse().unwrap()
    }

    fn now() -> u64 {
        0
    }

    fn assert_closed(hconn: &Http3Connection, expected: Error) {
        match hconn.state() {
            Http3State::Closing(err) | Http3State::Closed(err) => assert_eq!(err, expected.code()),
            _ => panic!("Wrong state {:?}", hconn.state()),
        };
    }

    // Start a client/server and check setting frame.
    fn connect(client: bool) -> (Http3Connection, Connection) {
        // Create a client/server and connect it to a server/client.
        // We will have a http3 server/client on one side and a neqo_transport
        // connection on the other side so that we can check what the http3
        // side sends and also to simulate an incorrectly behaving http3
        // server/client.

        init_db("./../neqo-transport/db");
        let mut hconn;
        let mut neqo_trans_conn;
        if client {
            hconn = Http3Connection::new(
                Connection::new_client("example.com", &["alpn"], loopback(), loopback()).unwrap(),
                100,
                100,
                None,
            );
            neqo_trans_conn = Connection::new_server(&["key"], &["alpn"]).unwrap();
        } else {
            hconn = Http3Connection::new(
                Connection::new_server(&["key"], &["alpn"]).unwrap(),
                100,
                100,
                None,
            );
            neqo_trans_conn =
                Connection::new_client("example.com", &["alpn"], loopback(), loopback()).unwrap();
        }
        if client {
            assert_eq!(hconn.state(), Http3State::Initializing);
            let mut r = hconn.process(vec![], now());
            r = neqo_trans_conn.process(r.0, now());
            r = hconn.process(r.0, now());
            neqo_trans_conn.process(r.0, now());
            assert_eq!(hconn.state(), Http3State::Connected);
        } else {
            assert_eq!(hconn.state(), Http3State::Initializing);
            let mut r = neqo_trans_conn.process(vec![], now());
            r = hconn.process(r.0, now());
            r = neqo_trans_conn.process(r.0, now());
            r = hconn.process(r.0, now());
            assert_eq!(hconn.state(), Http3State::Connected);
            neqo_trans_conn.process(r.0, now());
        }

        let events = neqo_trans_conn.events();
        for e in events {
            match e {
                ConnectionEvent::NewStream {
                    stream_id,
                    stream_type,
                } => {
                    assert!(
                        (client && ((stream_id == 2) || (stream_id == 6) || (stream_id == 10)))
                            || ((stream_id == 3) || (stream_id == 7) || (stream_id == 11))
                    );
                    assert_eq!(stream_type, StreamType::UniDi);
                }
                ConnectionEvent::RecvStreamReadable { stream_id } => {
                    if stream_id == 2 || stream_id == 3 {
                        // the control stream
                        let mut buf = [0u8; 100];
                        let (amount, fin) =
                            neqo_trans_conn.stream_recv(stream_id, &mut buf).unwrap();
                        assert_eq!(fin, false);
                        assert_eq!(amount, 9);
                        assert_eq!(buf[..9], [0x0, 0x4, 0x6, 0x1, 0x40, 0x64, 0x7, 0x40, 0x64]);
                    } else if stream_id == 6 || stream_id == 7 {
                        let mut buf = [0u8; 100];
                        let (amount, fin) =
                            neqo_trans_conn.stream_recv(stream_id, &mut buf).unwrap();
                        assert_eq!(fin, false);
                        assert_eq!(amount, 1);
                        assert_eq!(buf[..1], [0x2]);
                    } else if stream_id == 10 || stream_id == 11 {
                        let mut buf = [0u8; 100];
                        let (amount, fin) =
                            neqo_trans_conn.stream_recv(stream_id, &mut buf).unwrap();
                        assert_eq!(fin, false);
                        assert_eq!(amount, 1);
                        assert_eq!(buf[..1], [0x3]);
                    } else {
                        assert!(false, "unexpected event");
                    }
                }
                ConnectionEvent::SendStreamWritable { stream_id } => {
                    assert!((stream_id == 2) || (stream_id == 6) || (stream_id == 10));
                }
                _ => assert!(false, "unexpected event"),
            }
        }
        (hconn, neqo_trans_conn)
    }

    // Test http3 connection inintialization.
    // The client will open the control and qpack streams and send SETTINGS frame.
    #[test]
    fn test_client_connect() {
        let _ = connect(true);
    }

    // Test http3 connection inintialization.
    // The server will open the control and qpack streams and send SETTINGS frame.
    #[test]
    fn test_server_connect() {
        let _ = connect(false);
    }

    fn connect_and_receive_control_stream(client: bool) -> (Http3Connection, Connection, u64) {
        let (mut hconn, mut neqo_trans_conn) = connect(client);
        let control_stream = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();
        let mut sent = neqo_trans_conn.stream_send(
            control_stream,
            &[0x0, 0x4, 0x6, 0x1, 0x40, 0x64, 0x7, 0x40, 0x64],
        );
        assert_eq!(sent, Ok(9));
        let encoder_stream = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();
        sent = neqo_trans_conn.stream_send(encoder_stream, &[0x2]);
        assert_eq!(sent, Ok(1));
        let decoder_stream = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();
        sent = neqo_trans_conn.stream_send(decoder_stream, &[0x3]);
        assert_eq!(sent, Ok(1));
        let r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        // assert no error occured.
        assert_eq!(hconn.state(), Http3State::Connected);
        (hconn, neqo_trans_conn, control_stream)
    }

    // Client: Test receiving a new control stream and a SETTINGS frame.
    #[test]
    fn test_client_receive_control_frame() {
        let _ = connect_and_receive_control_stream(true);
    }

    // Server: Test receiving a new control stream and a SETTINGS frame.
    #[test]
    fn test_server_receive_control_frame() {
        let _ = connect_and_receive_control_stream(false);
    }

    // Client: Test that the connection will be closed if control stream
    // has been closed.
    #[test]
    fn test_client_close_control_stream() {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(true);
        neqo_trans_conn.stream_close_send(3).unwrap();
        let r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());
        assert_closed(&hconn, Error::ClosedCriticalStream);
    }

    // Server: Test that the connection will be closed if control stream
    // has been closed.
    #[test]
    fn test_server_close_control_stream() {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(false);
        neqo_trans_conn.stream_close_send(2).unwrap();
        let r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());
        assert_closed(&hconn, Error::ClosedCriticalStream);
    }

    // Client: test missing SETTINGS frame
    // (the first frame sent is a PRIORITY frame).
    #[test]
    fn test_client_missing_settings() {
        let (mut hconn, mut neqo_trans_conn) = connect(true);
        // create server control stream.
        let control_stream = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();
        // send a PRIORITY frame.
        let sent =
            neqo_trans_conn.stream_send(control_stream, &[0x0, 0x2, 0x4, 0x0, 0x2, 0x1, 0x3]);
        assert_eq!(sent, Ok(7));
        let r = neqo_trans_conn.process(Vec::new(), 0);
        hconn.process(r.0, 0);
        assert_closed(&hconn, Error::MissingSettings);
    }

    // Server: test missing SETTINGS frame
    // (the first frame sent is a PRIORITY frame).
    #[test]
    fn test_server_missing_settings() {
        let (mut hconn, mut neqo_trans_conn) = connect(false);
        // create server control stream.
        let control_stream = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();
        // send a PRIORITY frame.
        let sent =
            neqo_trans_conn.stream_send(control_stream, &[0x0, 0x2, 0x4, 0x0, 0x2, 0x1, 0x3]);
        assert_eq!(sent, Ok(7));
        let r = neqo_trans_conn.process(Vec::new(), 0);
        hconn.process(r.0, 0);
        assert_closed(&hconn, Error::MissingSettings);
    }

    // Client: receiving SETTINGS frame twice causes connection close
    // with error HTTP_UNEXPECTED_FRAME.
    #[test]
    fn test_client_receive_settings_twice() {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(true);
        // send the second SETTINGS frame.
        let sent = neqo_trans_conn.stream_send(3, &[0x4, 0x6, 0x1, 0x40, 0x64, 0x7, 0x40, 0x64]);
        assert_eq!(sent, Ok(8));
        let r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());
        assert_closed(&hconn, Error::UnexpectedFrame);
    }

    // Server: receiving SETTINGS frame twice causes connection close
    // with error HTTP_UNEXPECTED_FRAME.
    #[test]
    fn test_server_receive_settings_twice() {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(false);
        // send the second SETTINGS frame.
        let sent = neqo_trans_conn.stream_send(2, &[0x4, 0x6, 0x1, 0x40, 0x64, 0x7, 0x40, 0x64]);
        assert_eq!(sent, Ok(8));
        let r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());
        assert_closed(&hconn, Error::UnexpectedFrame);
    }

    fn test_wrong_frame_on_control_stream(client: bool, v: &[u8]) {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(client);

        // receive a frame that is not allowed on the control stream.
        if client {
            let _ = neqo_trans_conn.stream_send(3, v);
        } else {
            let _ = neqo_trans_conn.stream_send(2, v);
        }

        let r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        assert_closed(&hconn, Error::WrongStream);
    }

    // send DATA frame on a cortrol stream
    #[test]
    fn test_data_frame_on_control_stream() {
        test_wrong_frame_on_control_stream(true, &[0x0, 0x2, 0x1, 0x2]);
        test_wrong_frame_on_control_stream(false, &[0x0, 0x2, 0x1, 0x2]);
    }

    // send HEADERS frame on a cortrol stream
    #[test]
    fn test_headers_frame_on_control_stream() {
        test_wrong_frame_on_control_stream(true, &[0x1, 0x2, 0x1, 0x2]);
        test_wrong_frame_on_control_stream(false, &[0x1, 0x2, 0x1, 0x2]);
    }

    // send PUSH_PROMISE frame on a cortrol stream
    #[test]
    fn test_push_promise_frame_on_control_stream() {
        test_wrong_frame_on_control_stream(true, &[0x5, 0x2, 0x1, 0x2]);
        test_wrong_frame_on_control_stream(false, &[0x5, 0x2, 0x1, 0x2]);
    }

    // send DUPLICATE_PUSH frame on a cortrol stream
    #[test]
    fn test_duplicate_push_frame_on_control_stream() {
        test_wrong_frame_on_control_stream(true, &[0xe, 0x2, 0x1, 0x2]);
        test_wrong_frame_on_control_stream(false, &[0xe, 0x2, 0x1, 0x2]);
    }

    // Client: receive unkonwn stream type
    // This function also tests getting stream id that does not fit into a single byte.
    #[test]
    fn test_client_received_unknown_stream() {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(true);

        // create a stream with unknown type.
        let new_stream_id = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();
        let _ = neqo_trans_conn.stream_send(
            new_stream_id,
            &vec![0x41, 0x19, 0x4, 0x4, 0x6, 0x0, 0x8, 0x0],
        );
        let mut r = neqo_trans_conn.process(vec![], now());
        r = hconn.process(r.0, now());
        neqo_trans_conn.process(r.0, now());

        // check for stop-sending with Error::UnknownStreamType.
        let events = neqo_trans_conn.events();
        let mut stop_sending_event_found = false;
        for e in events {
            match e {
                ConnectionEvent::SendStreamStopSending {
                    stream_id,
                    app_error,
                } => {
                    stop_sending_event_found = true;
                    assert_eq!(stream_id, new_stream_id);
                    assert_eq!(app_error, Error::UnknownStreamType.code());
                }
                _ => {}
            }
        }
        assert!(stop_sending_event_found);
        assert_eq!(hconn.state(), Http3State::Connected);
    }

    // Server: receive unkonwn stream type
    // also test getting stream id that does not fit into a single byte.
    #[test]
    fn test_server_received_unknown_stream() {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(false);

        // create a stream with unknown type.
        let new_stream_id = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();
        let _ = neqo_trans_conn.stream_send(
            new_stream_id,
            &vec![0x41, 0x19, 0x4, 0x4, 0x6, 0x0, 0x8, 0x0],
        );
        let mut r = neqo_trans_conn.process(vec![], now());
        r = hconn.process(r.0, now());
        neqo_trans_conn.process(r.0, now());

        // check for stop-sending with Error::UnknownStreamType.
        let events = neqo_trans_conn.events();
        let mut stop_sending_event_found = false;
        for e in events {
            match e {
                ConnectionEvent::SendStreamStopSending {
                    stream_id,
                    app_error,
                } => {
                    stop_sending_event_found = true;
                    assert_eq!(stream_id, new_stream_id);
                    assert_eq!(app_error, Error::UnknownStreamType.code());
                }
                _ => {}
            }
        }
        assert!(stop_sending_event_found);
        assert_eq!(hconn.state(), Http3State::Connected);
    }

    // Client: receive a push stream
    #[test]
    fn test_client_received_push_stream() {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(true);

        // create a push stream.
        let push_stream_id = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();
        let _ = neqo_trans_conn.stream_send(push_stream_id, &vec![0x1]);
        let mut r = neqo_trans_conn.process(vec![], now());
        r = hconn.process(r.0, now());
        neqo_trans_conn.process(r.0, now());

        // check for stop-sending with Error::Error::PushRefused.
        let events = neqo_trans_conn.events();
        let mut stop_sending_event_found = false;
        for e in events {
            match e {
                ConnectionEvent::SendStreamStopSending {
                    stream_id,
                    app_error,
                } => {
                    stop_sending_event_found = true;
                    assert_eq!(stream_id, push_stream_id);
                    assert_eq!(app_error, Error::PushRefused.code());
                }
                _ => {}
            }
        }
        assert!(stop_sending_event_found);
        assert_eq!(hconn.state(), Http3State::Connected);
    }

    // Server: receiving a push stream on a server should cause WrongStreamDirection
    #[test]
    fn test_server_received_push_stream() {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(false);

        // create a push stream.
        let push_stream_id = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();
        let _ = neqo_trans_conn.stream_send(push_stream_id, &vec![0x1]);
        let mut r = neqo_trans_conn.process(vec![], now());
        r = hconn.process(r.0, now());
        neqo_trans_conn.process(r.0, now());

        // check for stop-sending with Error::WrongStreamDirection.
        let events = neqo_trans_conn.events();
        let mut stop_sending_event_found = false;
        for e in events {
            match e {
                ConnectionEvent::SendStreamStopSending {
                    stream_id,
                    app_error,
                } => {
                    stop_sending_event_found = true;
                    assert_eq!(stream_id, push_stream_id);
                    assert_eq!(app_error, Error::WrongStreamDirection.code());
                }
                _ => {}
            }
        }
        assert!(stop_sending_event_found);
        assert_eq!(hconn.state(), Http3State::Connected);
    }

    // Test wrong frame on req/rec stream
    fn test_wrong_frame_on_request_stream(v: &[u8], err: Error) {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(true);

        assert_eq!(
            hconn.fetch(
                &"GET".to_string(),
                &"https".to_string(),
                &"something.com".to_string(),
                &"/".to_string(),
                &Vec::<(String, String)>::new()
            ),
            Ok(0)
        );

        let mut r = hconn.process(vec![], 0);
        neqo_trans_conn.process(r.0, now());

        // find the new request/response stream and send frame v on it.
        let events = neqo_trans_conn.events();
        for e in events {
            match e {
                ConnectionEvent::NewStream {
                    stream_id,
                    stream_type,
                } => {
                    assert_eq!(stream_type, StreamType::BiDi);
                    let _ = neqo_trans_conn.stream_send(stream_id, v);
                }
                _ => {}
            }
        }
        // Generate packet with the above bad h3 input
        r = neqo_trans_conn.process(vec![], now());
        // Process bad input and generate stop sending frame
        r = hconn.process(r.0, 0);
        // Process stop sending frame and generate an event and a reset frame
        r = neqo_trans_conn.process(r.0, now());

        let mut stop_sending_event_found = false;
        for e in neqo_trans_conn.events() {
            match e {
                ConnectionEvent::SendStreamStopSending {
                    stream_id,
                    app_error,
                } => {
                    assert_eq!(stream_id, 0);
                    stop_sending_event_found = true;
                    assert_eq!(app_error, err.code());
                }
                _ => {}
            }
        }
        assert!(stop_sending_event_found);
        assert_eq!(hconn.state(), Http3State::Connected);

        // Process reset frame
        hconn.conn.process(r.0, 0);
        let mut reset_event_found = false;
        for e in hconn.conn.events() {
            match e {
                ConnectionEvent::RecvStreamReset {
                    stream_id: _,
                    app_error,
                } => {
                    reset_event_found = true;
                    assert_eq!(app_error, err.code());
                }
                _ => {}
            }
        }
        assert!(reset_event_found);
        assert_eq!(hconn.state(), Http3State::Connected);
    }

    #[test]
    fn test_cancel_push_frame_on_request_stream() {
        test_wrong_frame_on_request_stream(&vec![0x3, 0x1, 0x5], Error::WrongStream);
    }

    #[test]
    fn test_settings_frame_on_request_stream() {
        test_wrong_frame_on_request_stream(&vec![0x4, 0x4, 0x6, 0x4, 0x8, 0x4], Error::WrongStream);
    }

    #[test]
    fn test_goaway_frame_on_request_stream() {
        test_wrong_frame_on_request_stream(&vec![0x7, 0x1, 0x5], Error::WrongStream);
    }

    #[test]
    fn test_max_push_id_frame_on_request_stream() {
        test_wrong_frame_on_request_stream(&vec![0xd, 0x1, 0x5], Error::WrongStream);
    }

    #[test]
    fn test_priority_frame_on_client_on_request_stream() {
        test_wrong_frame_on_request_stream(
            &vec![0x2, 0x4, 0xf, 0x2, 0x1, 0x3],
            Error::UnexpectedFrame,
        );
    }

    // Test reading of a slowly streamed frame. bytes are received one by one
    #[test]
    fn test_frame_reading() {
        let (mut hconn, mut neqo_trans_conn) = connect(true);

        // create a control stream.
        let control_stream = neqo_trans_conn.stream_create(StreamType::UniDi).unwrap();

        // send the stream type
        let mut sent = neqo_trans_conn.stream_send(control_stream, &[0x0]);
        assert_eq!(sent, Ok(1));
        let mut r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        // start sending SETTINGS frame
        sent = neqo_trans_conn.stream_send(control_stream, &[0x4]);
        assert_eq!(sent, Ok(1));
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        sent = neqo_trans_conn.stream_send(control_stream, &[0x4]);
        assert_eq!(sent, Ok(1));
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        sent = neqo_trans_conn.stream_send(control_stream, &[0x6]);
        assert_eq!(sent, Ok(1));
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        sent = neqo_trans_conn.stream_send(control_stream, &[0x0]);
        assert_eq!(sent, Ok(1));
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        sent = neqo_trans_conn.stream_send(control_stream, &[0x8]);
        assert_eq!(sent, Ok(1));
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        sent = neqo_trans_conn.stream_send(control_stream, &[0x0]);
        assert_eq!(sent, Ok(1));
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        assert_eq!(hconn.state(), Http3State::Connected);

        // Now test PushPromise
        sent = neqo_trans_conn.stream_send(control_stream, &[0x5]);
        assert_eq!(sent, Ok(1));
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        sent = neqo_trans_conn.stream_send(control_stream, &[0x5]);
        assert_eq!(sent, Ok(1));
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        sent = neqo_trans_conn.stream_send(control_stream, &[0x4]);
        assert_eq!(sent, Ok(1));
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, now());

        // PUSH_PROMISE on a control stream will cause an error
        assert_closed(&hconn, Error::WrongStream);
    }

    #[test]
    fn fetch() {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(true);
        let request_stream_id = hconn
            .fetch(
                &"GET".to_string(),
                &"https".to_string(),
                &"something.com".to_string(),
                &"/".to_string(),
                &Vec::<(String, String)>::new(),
            )
            .unwrap();
        assert_eq!(request_stream_id, 0);

        let mut r = hconn.process(vec![], 0);
        neqo_trans_conn.process(r.0, now());

        // find the new request/response stream and send frame v on it.
        let events = neqo_trans_conn.events();
        for e in events {
            match e {
                ConnectionEvent::NewStream {
                    stream_id,
                    stream_type,
                } => {
                    assert_eq!(stream_id, request_stream_id);
                    assert_eq!(stream_type, StreamType::BiDi);
                }
                ConnectionEvent::RecvStreamReadable { stream_id } => {
                    assert_eq!(stream_id, request_stream_id);
                    let mut buf = [0u8; 100];
                    let (amount, fin) = neqo_trans_conn.stream_recv(stream_id, &mut buf).unwrap();
                    assert_eq!(fin, true);
                    assert_eq!(amount, 18);
                    assert_eq!(
                        buf[..18],
                        [
                            0x01, 0x10, 0x00, 0x00, 0xd1, 0xd7, 0x50, 0x89, 0x41, 0xe9, 0x2a, 0x67,
                            0x35, 0x53, 0x2e, 0x43, 0xd3, 0xc1
                        ]
                    );
                    // send response - 200  Content-Length: 6
                    // with content: 'abcdef'.
                    // The content will be send in 2 DATA frames.
                    let _ = neqo_trans_conn.stream_send(
                        stream_id,
                        &[
                            // headers
                            0x01, 0x06, 0x00, 0x00, 0xd9, 0x54, 0x01, 0x33,
                            // the first data frame
                            0x0, 0x3, 0x61, 0x62, 0x63,
                            // the second data frame
                            // the first data frame
                            0x0, 0x3, 0x64, 0x65, 0x66,
                        ],
                    );
                    neqo_trans_conn.stream_close_send(stream_id).unwrap();
                }
                _ => {}
            }
        }
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, 0);

        let http_events = hconn.events();
        for e in http_events {
            match e {
                Http3Event::HeaderReady { stream_id } => {
                    assert_eq!(stream_id, request_stream_id);
                    let h = hconn.get_headers(stream_id);
                    assert_eq!(
                        h,
                        Ok(Some(vec![
                            (String::from(":status"), String::from("200")),
                            (String::from("content-length"), String::from("3"))
                        ]))
                    );
                }
                Http3Event::DataReadable { stream_id } => {
                    assert_eq!(stream_id, request_stream_id);
                    let mut buf = [0u8; 100];
                    let (amount, fin) = hconn.read_data(stream_id, &mut buf).unwrap();
                    assert_eq!(fin, false);
                    assert_eq!(amount, 3);
                    assert_eq!(buf[..3], [0x61, 0x62, 0x63]);
                }
                _ => {
                    assert! {false}
                }
            }
        }

        hconn.process_http3();
        let http_events = hconn.events();
        for e in http_events {
            match e {
                Http3Event::DataReadable { stream_id } => {
                    assert_eq!(stream_id, request_stream_id);
                    let mut buf = [0u8; 100];
                    let (amount, fin) = hconn.read_data(stream_id, &mut buf).unwrap();
                    assert_eq!(fin, true);
                    assert_eq!(amount, 3);
                    assert_eq!(buf[..3], [0x64, 0x65, 0x66]);
                }
                _ => {
                    assert! {false}
                }
            }
        }

        // after this stream will be removed from hcoon. We will check this by trying to read
        // from the stream and that should fail.
        let mut buf = [0u8; 100];
        if let Err(e) = hconn.read_data(request_stream_id, &mut buf) {
            assert_eq!(e, Http3Error::InvalidStreamId);
        } else {
            assert!(false);
        }

        hconn.close(0, String::from(""));
    }

    fn test_incomplet_frame(res: &[u8], error: Error) {
        let (mut hconn, mut neqo_trans_conn, _) = connect_and_receive_control_stream(true);
        let request_stream_id = hconn
            .fetch(
                &"GET".to_string(),
                &"https".to_string(),
                &"something.com".to_string(),
                &"/".to_string(),
                &Vec::<(String, String)>::new(),
            )
            .unwrap();
        assert_eq!(request_stream_id, 0);

        let mut r = hconn.process(vec![], 0);
        neqo_trans_conn.process(r.0, now());

        // find the new request/response stream and send frame v on it.
        let events = neqo_trans_conn.events();
        for e in events {
            match e {
                ConnectionEvent::NewStream {
                    stream_id,
                    stream_type,
                } => {
                    assert_eq!(stream_id, request_stream_id);
                    assert_eq!(stream_type, StreamType::BiDi);
                }
                ConnectionEvent::RecvStreamReadable { stream_id } => {
                    assert_eq!(stream_id, request_stream_id);
                    let mut buf = [0u8; 100];
                    let (amount, fin) = neqo_trans_conn.stream_recv(stream_id, &mut buf).unwrap();
                    assert_eq!(fin, true);
                    assert_eq!(amount, 18);
                    assert_eq!(
                        buf[..18],
                        [
                            0x01, 0x10, 0x00, 0x00, 0xd1, 0xd7, 0x50, 0x89, 0x41, 0xe9, 0x2a, 0x67,
                            0x35, 0x53, 0x2e, 0x43, 0xd3, 0xc1
                        ]
                    );
                    // send an incomplete response - 200  Content-Length: 3
                    // with content: 'abc'.
                    let _ = neqo_trans_conn.stream_send(stream_id, res);
                    neqo_trans_conn.stream_close_send(stream_id).unwrap();
                }
                _ => {}
            }
        }
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, 0);

        let http_events = hconn.events();
        for e in http_events {
            match e {
                Http3Event::DataReadable { stream_id } => {
                    assert_eq!(stream_id, request_stream_id);
                    let mut buf = [0u8; 100];
                    match hconn.read_data(stream_id, &mut buf) {
                        Err(e) => {
                            assert_eq!(e, Http3Error::ConnectionError);
                        }
                        Ok(_) => assert!(false),
                    }
                }
                _ => {}
            }
        }
        assert_closed(&hconn, error);
    }

    use crate::hframe::H3_FRAME_TYPE_DATA;
    use crate::hframe::H3_FRAME_TYPE_HEADERS;

    // Incomplete DATA frame
    #[test]
    fn test_incomplet_data_frame() {
        test_incomplet_frame(
            &[
                // headers
                0x01, 0x06, 0x00, 0x00, 0xd9, 0x54, 0x01, 0x33,
                // the data frame is incomplete.
                0x0, 0x3, 0x61, 0x62,
            ],
            Error::MalformedFrame(H3_FRAME_TYPE_DATA),
        );
    }

    // Incomplete HEADERS frame
    #[test]
    fn test_incomplet_headers_frame() {
        test_incomplet_frame(
            &[
                // headers
                0x01, 0x06, 0x00, 0x00, 0xd9, 0x54, 0x01,
            ],
            Error::MalformedFrame(H3_FRAME_TYPE_HEADERS),
        );
    }

    #[test]
    fn test_incomplet_unknown_frame() {
        test_incomplet_frame(&[0x21], Error::MalformedFrame(0xff));
    }

    // test goaway
    #[test]
    fn test_goaway() {
        let (mut hconn, mut neqo_trans_conn, _control_stream) =
            connect_and_receive_control_stream(true);
        let request_stream_id_1 = hconn
            .fetch(
                &"GET".to_string(),
                &"https".to_string(),
                &"something.com".to_string(),
                &"/".to_string(),
                &Vec::<(String, String)>::new(),
            )
            .unwrap();
        assert_eq!(request_stream_id_1, 0);
        let request_stream_id_2 = hconn
            .fetch(
                &"GET".to_string(),
                &"https".to_string(),
                &"something.com".to_string(),
                &"/".to_string(),
                &Vec::<(String, String)>::new(),
            )
            .unwrap();
        assert_eq!(request_stream_id_2, 4);
        let request_stream_id_3 = hconn
            .fetch(
                &"GET".to_string(),
                &"https".to_string(),
                &"something.com".to_string(),
                &"/".to_string(),
                &Vec::<(String, String)>::new(),
            )
            .unwrap();
        assert_eq!(request_stream_id_3, 8);

        let mut r = hconn.process(vec![], 0);
        neqo_trans_conn.process(r.0, now());

        let _ = neqo_trans_conn.stream_send(
            3, //control_stream,
            &[0x7, 0x1, 0x8],
        );

        // find the new request/response stream and send frame v on it.
        let events = neqo_trans_conn.events();
        for e in events {
            match e {
                ConnectionEvent::NewStream { .. } => {}
                ConnectionEvent::RecvStreamReadable { stream_id } => {
                    let mut buf = [0u8; 100];
                    let _ = neqo_trans_conn.stream_recv(stream_id, &mut buf).unwrap();
                    if stream_id == request_stream_id_1 || stream_id == request_stream_id_2 {
                        // send response - 200  Content-Length: 6
                        // with content: 'abcdef'.
                        // The content will be send in 2 DATA frames.
                        let _ = neqo_trans_conn.stream_send(
                            stream_id,
                            &[
                                // headers
                                0x01, 0x06, 0x00, 0x00, 0xd9, 0x54, 0x01, 0x33,
                                // the first data frame
                                0x0, 0x3, 0x61, 0x62, 0x63,
                                // the second data frame
                                // the first data frame
                                0x0, 0x3, 0x64, 0x65, 0x66,
                            ],
                        );

                        neqo_trans_conn.stream_close_send(stream_id).unwrap();
                    }
                }
                _ => {}
            }
        }
        r = neqo_trans_conn.process(vec![], now());
        hconn.process(r.0, 0);

        let mut stream_reset = false;
        let mut http_events = hconn.events();
        while http_events.len() > 0 {
            for e in http_events {
                match e {
                    Http3Event::HeaderReady { stream_id } => {
                        let h = hconn.get_headers(stream_id);
                        assert_eq!(
                            h,
                            Ok(Some(vec![
                                (String::from(":status"), String::from("200")),
                                (String::from("content-length"), String::from("3"))
                            ]))
                        );
                    }
                    Http3Event::DataReadable { stream_id } => {
                        assert!(
                            stream_id == request_stream_id_1 || stream_id == request_stream_id_2
                        );
                        let mut buf = [0u8; 100];
                        let (amount, _) = hconn.read_data(stream_id, &mut buf).unwrap();
                        assert_eq!(amount, 3);
                    }
                    Http3Event::RequestClosed { stream_id, error } => {
                        assert!(stream_id == request_stream_id_3);
                        assert_eq!(error, Http3Error::NetReset);
                        stream_reset = true;
                    }
                    _ => {
                        assert! {false}
                    }
                }
            }
            hconn.process_http3();
            http_events = hconn.events();
        }

        assert!(stream_reset);
        assert_eq!(hconn.state(), Http3State::GoingAway);
        hconn.close(0, String::from(""));
    }
}
