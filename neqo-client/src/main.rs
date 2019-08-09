// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![deny(warnings)]
use neqo_common::Datagram;
use neqo_crypto::init_db;
use neqo_http3::{Http3Connection, Http3Event, Http3State};
use neqo_transport::Connection;
use std::collections::HashSet;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;
use std::string::ParseError;
use std::time::Instant;
use structopt::StructOpt;
use url::Url;

#[derive(Debug)]
struct Headers {
    pub h: Vec<(String, String)>,
}

// dragana: this is a very stupid parser.
// headers should be in form "[(something1, something2), (something3, something4)]"
impl FromStr for Headers {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut res = Headers { h: Vec::new() };
        let h1: Vec<&str> = s
            .trim_matches(|p| p == '[' || p == ']')
            .split(')')
            .collect();

        for h in h1 {
            let h2: Vec<&str> = h
                .trim_matches(|p| p == ',')
                .trim()
                .trim_matches(|p| p == '(' || p == ')')
                .split(',')
                .collect();

            if h2.len() == 2 {
                res.h
                    .push((h2[0].trim().to_string(), h2[1].trim().to_string()));
            }
        }

        Ok(res)
    }
}

#[derive(Debug, StructOpt)]
#[structopt(
    name = "neqo-client",
    about = "A basic QUIC HTTP/0.9 and HTTP3 client."
)]
pub struct Args {
    #[structopt(short = "d", long, default_value = "./db", parse(from_os_str))]
    db: PathBuf,

    #[structopt(short = "a", long, default_value = "h3-22")]
    /// ALPN labels to negotiate.
    ///
    /// This client still only does HTTP3 no matter what the ALPN says.
    alpn: Vec<String>,

    url: Url,

    #[structopt(short = "m", default_value = "GET")]
    method: String,

    #[structopt(short = "h", long, default_value = "[]")]
    headers: Headers,

    #[structopt(name = "max-table-size", short = "t", long, default_value = "128")]
    max_table_size: u32,

    #[structopt(name = "max-blocked-streams", short = "b", long, default_value = "128")]
    max_blocked_streams: u16,

    #[structopt(name = "use-old-http", short = "o", long)]
    /// Use http 0.9 instead of HTTP/3
    use_old_http: bool,

    #[structopt(name = "omit-read-data", long)]
    /// Do not print received data
    omit_read_data: bool,
}

impl Args {
    fn remote_addr(&self) -> Result<SocketAddr, io::Error> {
        Ok(self.to_socket_addrs()?.next().expect("No remote addresses"))
    }

    fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        match self.remote_addr()? {
            SocketAddr::V4(..) => Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from([0; 4])), 0)),
            SocketAddr::V6(..) => Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from([0; 16])), 0)),
        }
    }
}

impl ToSocketAddrs for Args {
    type Iter = ::std::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> ::std::io::Result<Self::Iter> {
        // This is idiotic.  There is no path from hostname: String to IpAddr.
        // And no means of controlling name resolution either.
        if self.url.port_or_known_default().is_none() {
            return Err(io::Error::new(ErrorKind::InvalidInput, "invalid port"));
        }
        std::fmt::format(format_args!(
            "{}:{}",
            self.url.host_str().unwrap_or("localhost"),
            self.url.port_or_known_default().unwrap()
        ))
        .to_socket_addrs()
    }
}

trait Handler {
    fn handle(&mut self, args: &Args, client: &mut Http3Connection) -> bool;
}

fn emit_datagram(socket: &UdpSocket, d: Option<Datagram>) {
    if let Some(d) = d {
        let sent = socket.send(&d[..]).expect("Error sending datagram");
        if sent != d.len() {
            eprintln!("Unable to send all {} bytes of datagram", d.len());
        }
    }
}

fn process_loop(
    local_addr: &SocketAddr,
    remote_addr: &SocketAddr,
    socket: &UdpSocket,
    client: &mut Http3Connection,
    handler: &mut Handler,
    args: &Args,
) -> neqo_http3::connection::Http3State {
    let buf = &mut [0u8; 2048];
    loop {
        if let Http3State::Closed(..) = client.state() {
            return client.state();
        }

        let exiting = !handler.handle(args, client);

        let out_dgram = client.process_output(Instant::now());
        emit_datagram(&socket, out_dgram.dgram());

        if exiting {
            return client.state();
        }

        let sz = match socket.recv(&mut buf[..]) {
            Err(err) => {
                eprintln!("UDP error: {}", err);
                exit(1)
            }
            Ok(sz) => sz,
        };
        if sz == buf.len() {
            eprintln!("Received more than {} bytes", buf.len());
            continue;
        }
        if sz > 0 {
            let d = Datagram::new(*remote_addr, *local_addr, &buf[..sz]);
            client.process_input(d, Instant::now());
        }
    }
}

struct PreConnectHandler {}
impl Handler for PreConnectHandler {
    fn handle(&mut self, _args: &Args, client: &mut Http3Connection) -> bool {
        Http3State::Connected != client.state()
    }
}

#[derive(Default)]
struct PostConnectHandler {
    streams: HashSet<u64>,
}

// This is a bit fancier than actually needed.
impl Handler for PostConnectHandler {
    fn handle(&mut self, args: &Args, client: &mut Http3Connection) -> bool {
        let mut data = vec![0; 4000];
        client.process_http3(Instant::now());
        for event in client.events() {
            match event {
                Http3Event::HeaderReady { stream_id } => {
                    if !self.streams.contains(&stream_id) {
                        println!("Data on unexpected stream: {}", stream_id);
                        return false;
                    }

                    let headers = client.get_response_headers(stream_id);
                    println!("READ HEADERS[{}]: {:?}", stream_id, headers);
                }
                Http3Event::DataReadable { stream_id } => {
                    if !self.streams.contains(&stream_id) {
                        println!("Data on unexpected stream: {}", stream_id);
                        return false;
                    }

                    let (sz, fin) = client
                        .read_response_data(Instant::now(), stream_id, &mut data)
                        .expect("Read should succeed");
                    if args.omit_read_data {
                        println!("READ[{}]: {} bytes", stream_id, sz);
                    } else {
                        println!(
                            "READ[{}]: {}",
                            stream_id,
                            String::from_utf8(data.clone()).unwrap()
                        )
                    }
                    if fin {
                        println!("<FIN[{}]>", stream_id);
                        client.close(Instant::now(), 0, "kthxbye!");
                        return false;
                    }
                }
                _ => {}
            }
        }

        true
    }
}

fn client(args: Args, socket: UdpSocket, local_addr: SocketAddr, remote_addr: SocketAddr) {
    let mut client = Http3Connection::new(
        Connection::new_client(
            args.url.host_str().unwrap(),
            &args.alpn,
            local_addr,
            remote_addr,
        )
        .expect("must succeed"),
        args.max_table_size,
        args.max_blocked_streams,
        None,
    );
    // Temporary here to help out the type inference engine
    let mut h = PreConnectHandler {};
    process_loop(
        &local_addr,
        &remote_addr,
        &socket,
        &mut client,
        &mut h,
        &args,
    );

    let client_stream_id = client
        .fetch(
            &args.method,
            &args.url.scheme(),
            &args.url.host_str().unwrap(),
            &args.url.path(),
            &args.headers.h,
        )
        .unwrap();

    let mut h2 = PostConnectHandler::default();
    h2.streams.insert(client_stream_id);
    process_loop(
        &local_addr,
        &remote_addr,
        &socket,
        &mut client,
        &mut h2,
        &args,
    );
}

fn main() {
    let args = Args::from_args();
    init_db(args.db.clone());

    let remote_addr = match args.remote_addr() {
        Err(e) => {
            eprintln!("Unable to resolve remote addr: {}", e);
            exit(1)
        }
        Ok(addr) => addr,
    };
    let socket = match args.local_addr().and_then(UdpSocket::bind) {
        Err(e) => {
            eprintln!("Unable to bind UDP socket: {}", e);
            exit(1)
        }
        Ok(s) => s,
    };
    socket.connect(&args).expect("Unable to connect UDP socket");

    let local_addr = socket.local_addr().expect("Socket local address not bound");

    println!("Client connecting: {:?} -> {:?}", local_addr, remote_addr);

    if args.use_old_http {
        old::old_client(args, socket, local_addr, remote_addr)
    } else {
        client(args, socket, local_addr, remote_addr)
    }
}

mod old {
    use std::collections::HashSet;
    use std::net::{SocketAddr, UdpSocket};
    use std::process::exit;
    use std::time::Instant;

    use neqo_common::Datagram;
    use neqo_transport::{Connection, ConnectionEvent, State, StreamType};

    use super::{emit_datagram, Args};

    trait HandlerOld {
        fn handle(&mut self, args: &Args, client: &mut Connection) -> bool;
    }

    struct PreConnectHandlerOld {}
    impl HandlerOld for PreConnectHandlerOld {
        fn handle(&mut self, _args: &Args, client: &mut Connection) -> bool {
            State::Connected != *dbg!(client.state())
        }
    }

    #[derive(Default)]
    struct PostConnectHandlerOld {
        streams: HashSet<u64>,
    }

    // This is a bit fancier than actually needed.
    impl HandlerOld for PostConnectHandlerOld {
        fn handle(&mut self, args: &Args, client: &mut Connection) -> bool {
            let mut data = vec![0; 4000];
            for event in client.events() {
                match event {
                    ConnectionEvent::RecvStreamReadable { stream_id } => {
                        if !self.streams.contains(&stream_id) {
                            println!("Data on unexpected stream: {}", stream_id);
                            return false;
                        }

                        let (sz, fin) = client
                            .stream_recv(stream_id, &mut data)
                            .expect("Read should succeed");
                        if args.omit_read_data {
                            println!("READ[{}]: {} bytes", stream_id, sz);
                        } else {
                            println!(
                                "READ[{}]: {}",
                                stream_id,
                                String::from_utf8(data.clone()).unwrap()
                            )
                        }
                        if fin {
                            println!("<FIN[{}]>", stream_id);
                            client.close(Instant::now(), 0, "kthxbye!");
                            return false;
                        }
                    }
                    ConnectionEvent::SendStreamWritable { stream_id } => {
                        println!("stream {} writable", stream_id)
                    }
                    _ => {
                        println!("Unexpected event {:?}", event);
                    }
                }
            }

            true
        }
    }

    fn process_loop_old(
        local_addr: &SocketAddr,
        remote_addr: &SocketAddr,
        socket: &UdpSocket,
        client: &mut Connection,
        handler: &mut HandlerOld,
        args: &Args,
    ) -> State {
        let buf = &mut [0u8; 2048];
        loop {
            if let State::Closed(..) = client.state() {
                return client.state().clone();
            }

            let exiting = !handler.handle(args, client);

            let out_dgram = client.process_output(Instant::now());
            emit_datagram(&socket, out_dgram.dgram());

            if exiting {
                return client.state().clone();
            }

            let sz = match socket.recv(&mut buf[..]) {
                Err(err) => {
                    eprintln!("UDP error: {}", err);
                    exit(1)
                }
                Ok(sz) => sz,
            };
            if sz == buf.len() {
                eprintln!("Received more than {} bytes", buf.len());
                continue;
            }
            if sz > 0 {
                let d = Datagram::new(*remote_addr, *local_addr, &buf[..sz]);
                client.process_input(d, Instant::now());
            }
        }
    }

    pub fn old_client(
        args: Args,
        socket: UdpSocket,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) {
        dbg!(args.url.host_str().unwrap());
        dbg!(&args.alpn);
        dbg!(local_addr);
        dbg!(remote_addr);

        let mut client = Connection::new_client(
            args.url.host_str().unwrap(),
            &["http/0.9"],
            local_addr,
            remote_addr,
        )
        .expect("must succeed");
        // Temporary here to help out the type inference engine
        let mut h = PreConnectHandlerOld {};
        process_loop_old(
            &local_addr,
            &remote_addr,
            &socket,
            &mut client,
            &mut h,
            &args,
        );

        let client_stream_id = client.stream_create(StreamType::BiDi).unwrap();
        let req: String = "GET /10\r\n".to_string();
        client
            .stream_send(client_stream_id, req.as_bytes())
            .unwrap();
        let mut h2 = PostConnectHandlerOld::default();
        h2.streams.insert(client_stream_id);
        process_loop_old(
            &local_addr,
            &remote_addr,
            &socket,
            &mut client,
            &mut h2,
            &args,
        );
    }
}
