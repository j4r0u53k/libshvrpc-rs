use std::{
    collections::hash_map::{Entry, HashMap},
};
use std::collections::BTreeMap;
use futures::{select, FutureExt, StreamExt};
use async_std::{channel, io::BufReader, net::{TcpListener, TcpStream, ToSocketAddrs}, prelude::*, task};
use rand::distributions::{Alphanumeric, DistString};
use log::*;
use structopt::StructOpt;
use shv::rpcframe::RpcFrame;
use shv::{RpcMessage, RpcValue};
use shv::rpcmessage::{CliId, RpcError, RpcErrorCode};
use shv::RpcMessageMetaTags;
use simple_logger::SimpleLogger;
use shv::shvnode::{find_longest_prefix, dir_ls, ShvNode, ProcessRequestResult};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Sender<T> = async_std::channel::Sender<T>;
type Receiver<T> = async_std::channel::Receiver<T>;

#[derive(Debug)]
enum Void {}

#[derive(StructOpt, Debug)]
#[structopt()]
struct Opt {
    /// Verbose mode (module, .)
    #[structopt(short = "v", long = "verbose")]
    verbose: Option<String>,
}

pub(crate) fn main() -> Result<()> {
    let opt = Opt::from_args();

    let mut logger = SimpleLogger::new();
    logger = logger.with_level(LevelFilter::Info);
    if let Some(module_names) = opt.verbose {
        for module_name in module_names.split(',') {
            let module_name = if module_name == "." {
                module_path!().to_string()
            } else {
                module_name.to_string()
            };
            logger = logger.with_module_level(&module_name, LevelFilter::Trace);
        }
    }
    logger.init().unwrap();

    trace!("trace message");
    debug!("debug message");
    info!("info message");
    warn!("warn message");
    error!("error message");
    log!(target: "RpcMsg", Level::Debug, "RPC message");

    let port = 3755;
    let host = "127.0.0.1";
    let address = format!("{}:{}", host, port);
    info!("Listening on: {}", &address);
    task::block_on(accept_loop(address))
}

async fn accept_loop(addr: impl ToSocketAddrs) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;

    let mut client_id = 0;
    let (broker_sender, broker_receiver) = channel::unbounded();
    let broker = task::spawn(broker_loop(broker_receiver));
    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        debug!("Accepting from: {}", stream.peer_addr()?);
        client_id += 1;
        spawn_and_log_error(client_loop(client_id, broker_sender.clone(), stream));
    }
    drop(broker_sender);
    broker.await;
    Ok(())
}
enum LoginResult {
    Ok,
    ClientSocketClosed,
    LoginError,
}
async fn send_error(mut response: RpcMessage, mut writer: &TcpStream, errmsg: &str) -> Result<()> {
    response.set_error(RpcError{ code: RpcErrorCode::MethodCallException, message: errmsg.into()});
    shv::connection::send_message(&mut writer, &response).await
}
async fn send_result(mut response: RpcMessage, mut writer: &TcpStream, result: RpcValue) -> Result<()> {
    response.set_result(result);
    shv::connection::send_message(&mut writer, &response).await
}
async fn client_loop(client_id: i32, broker_writer: Sender<ClientEvent>, stream: TcpStream) -> Result<()> {
    let (socket_reader, mut frame_writer) = (&stream, &stream);
    let (peer_writer, peer_receiver) = channel::unbounded::<PeerEvent>();

    broker_writer.send(ClientEvent::NewClient { client_id, sender: peer_writer }).await.unwrap();

    //let stream_wr = stream.clone();
    let mut brd = BufReader::new(socket_reader);
    let mut frame_reader = shv::connection::FrameReader::new(&mut brd);
    let mut device_options = RpcValue::null();
    let login_result = loop {
        let login_fut = async {
            let frame = match frame_reader.receive_frame().await? {
                None => return crate::Result::Ok(LoginResult::ClientSocketClosed),
                Some(frame) => { frame }
            };
            let rpcmsg = frame.to_rpcmesage()?;
            let resp = rpcmsg.prepare_response()?;
            if rpcmsg.method().unwrap_or("") == "hello" {
                let nonce = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
                let mut result = shv::Map::new();
                result.insert("nonce".into(), RpcValue::from(&nonce));
                send_result(resp, frame_writer, result.into()).await?;

                let frame = match frame_reader.receive_frame().await? {
                    None => return crate::Result::Ok(LoginResult::ClientSocketClosed),
                    Some(frame) => { frame }
                };
                let rpcmsg = frame.to_rpcmesage()?;
                let resp = rpcmsg.prepare_response()?;
                if rpcmsg.method().unwrap_or("") == "login" {
                    let params = rpcmsg.param().ok_or("No login params")?.as_map();
                    let login = params.get("login").ok_or("Invalid login params")?.as_map();
                    let user = login.get("user").ok_or("User login param is missing")?.as_str();
                    let password = login.get("password").ok_or("Password login param is missing")?.as_str();
                    let login_type = login.get("type").map(|v| v.as_str()).unwrap_or("");

                    broker_writer.send(ClientEvent::GetPassword { client_id, user: user.to_string() }).await.unwrap();
                    match peer_receiver.recv().await? {
                        PeerEvent::PasswordSha1(broker_shapass) => {
                            let chkpwd = || {
                                if login_type == "PLAIN" {
                                    let client_shapass = shv::connection::sha1_hash(password.as_bytes());
                                    client_shapass == broker_shapass
                                } else {
                                    //info!("nonce: {}", nonce);
                                    //info!("broker password: {}", std::str::from_utf8(&broker_shapass).unwrap());
                                    //info!("client password: {}", password);
                                    let mut data = nonce.as_bytes().to_vec();
                                    data.extend_from_slice(&broker_shapass[..]);
                                    let broker_shapass = shv::connection::sha1_hash(&data);
                                    password.as_bytes() == broker_shapass
                                }
                            };
                            if chkpwd() {
                                let mut result = shv::Map::new();
                                result.insert("clientId".into(), RpcValue::from(client_id));
                                send_result(resp, frame_writer, result.into()).await?;
                                if let Some(options) = params.get("options") {
                                    if let Some(device) = options.as_map().get("device") {
                                        device_options = device.clone();
                                    }
                                }
                                crate::Result::Ok(LoginResult::Ok)
                            } else {
                                send_error(resp, frame_writer, &format!("Invalid login credentials received.")).await?;
                                Ok(LoginResult::LoginError)
                            }
                        }
                        _ => {
                            panic!("Internal error, PeerEvent::PasswordSha1 expected");
                        }
                    }
                } else {
                    send_error(resp, frame_writer, &format!("login message expected.")).await?;
                    Ok(LoginResult::LoginError)
                }
            } else {
                send_error(resp, frame_writer, &format!("hello message expected.")).await?;
                Ok(LoginResult::LoginError)
            }
        };
        match login_fut.await {
            Ok(login_result) => {
                break login_result;
            }
            Err(errmsg) => {
                warn!("{}", errmsg);
            }
        }
    };
    if let LoginResult::Ok = login_result {
        let register_device = ClientEvent::RegiterDevice {
            client_id,
            device_id: device_options.as_map().get("deviceId").map(|v| v.as_str().to_string()),
            mount_point: device_options.as_map().get("mountPoint").map(|v| v.as_str().to_string()),
        };
        broker_writer.send(register_device).await.unwrap();
        loop {
            select! {
                frame = frame_reader.receive_frame().fuse() => match frame {
                    Ok(frame) => {
                        match frame {
                            None => {
                                debug!("Client socket closed");
                                break;
                            }
                            Some(frame) => {
                                // log!(target: "RpcMsg", Level::Debug, "----> Recv frame, client id: {}", client_id);
                                broker_writer.send(ClientEvent::Frame { client_id, frame }).await.unwrap();
                            }
                        }
                    }
                    Err(e) => {
                        error!("Read socket error: {}", &e);
                        break;
                    }
                },
                event = peer_receiver.recv().fuse() => match event {
                    Err(e) => {
                        debug!("Peer channel closed: {}", &e);
                        break;
                    }
                    Ok(event) => {
                        match event {
                            PeerEvent::PasswordSha1(_) => {
                                panic!("PasswordSha1 cannot be received here")
                            }
                            //PeerEvent::FatalError(errmsg) => {
                            //    error!("Fatal client error: {}", errmsg);
                            //    break;
                            //}
                            PeerEvent::Frame(frame) => {
                                // log!(target: "RpcMsg", Level::Debug, "<---- Send frame, client id: {}", client_id);
                                shv::connection::send_frame(&mut frame_writer, frame).await?;
                            }
                            PeerEvent::Message(rpcmsg) => {
                                // log!(target: "RpcMsg", Level::Debug, "<---- Send message, client id: {}", client_id);
                                shv::connection::send_message(&mut frame_writer, &rpcmsg).await?;
                            },
                        }
                    }
                }
            }
        }
    }
    broker_writer.send(ClientEvent::ClientGone { client_id }).await.unwrap();
    info!("Client loop exit, client id: {}", client_id);
    Ok(())
}

#[derive(Debug)]
enum ClientEvent {
    GetPassword {
        client_id: CliId,
        user: String,
    },
    NewClient {
        client_id: CliId,
        sender: Sender<PeerEvent>,
    },
    RegiterDevice {
        client_id: CliId,
        device_id: Option<String>,
        mount_point: Option<String>,
    },
    Frame {
        client_id: CliId,
        frame: RpcFrame,
    },
    ClientGone {
        client_id: CliId,
    },
}

#[derive(Debug)]
enum PeerEvent {
    PasswordSha1(Vec<u8>),
    Frame(RpcFrame),
    Message(RpcMessage),
    //FatalError(String),
}
#[derive(Debug)]
struct Peer {
    sender: Sender<PeerEvent>,
}
struct Device {
    client_id: CliId,
}
type Node = Box<dyn ShvNode + Send + Sync>;
enum Mount {
    Device(Device),
    Node(Node),
}
struct Broker {
    peers: HashMap<CliId, Peer>,
    mounts: BTreeMap<String, Mount>,
}

impl Broker {
    pub fn find_mount<'a, 'b>(&'a mut self, shv_path: &'b str) -> Option<(&'a mut Mount, &'b str)> {
        if let Some((mount_dir, node_dir)) = find_longest_prefix(&self.mounts, shv_path) {
            Some((self.mounts.get_mut(mount_dir).unwrap(), node_dir))
        } else {
            None
        }
    }
    pub fn sha_password(&self, user: &str) -> Vec<u8> {
        shv::connection::sha1_hash(user.as_bytes())
    }
    pub fn mount_device(&mut self, client_id: i32, device_id: Option<String>, mount_point: Option<String>) {
        if let Some(mount_point) = mount_point {
            if mount_point.starts_with("test/") {
                info!("Client id: {} mounted on path: '{}'", client_id, &mount_point);
                self.mounts.insert(mount_point, Mount::Device(Device { client_id }));
                return;
            }
        }
        if let Some(device_id) = device_id {
            let mount_point = "test/".to_owned() + &device_id;
            info!("Client id: {}, device id: {} mounted on path: '{}'", client_id, device_id, &mount_point);
            self.mounts.insert(mount_point, Mount::Device(Device { client_id }));
            return;
        }
    }
}
async fn broker_loop(events: Receiver<ClientEvent>) {
    let mut broker = Broker {
        peers: HashMap::new(),
        mounts: BTreeMap::new(),
    };
    broker.mounts.insert(".app".into(), Mount::Node(Box::new(shv::shvnode::AppNode { app_name: "shvbroker", ..Default::default() })));
    loop {
        match events.recv().await {
            Err(e) => {
                info!("Client channel closed: {}", &e);
                break;
            }
            Ok(event) => {
                match event {
                    ClientEvent::Frame { client_id, mut frame} => {
                        if frame.is_request() {
                            let shv_path = frame.shv_path().unwrap_or("").to_string();
                            let response_meta= RpcFrame::prepare_response_meta(&frame.meta);
                            let result: Option<ProcessRequestResult> = if let Some((mount, node_path)) = broker.find_mount(&shv_path) {
                                match mount {
                                    Mount::Device(device) => {
                                        let device_client_id = device.client_id;
                                        frame.push_caller_id(client_id);
                                        frame.set_shvpath(node_path);
                                        let _ = broker.peers.get(&device_client_id).unwrap().sender.send(PeerEvent::Frame(frame)).await;
                                        None
                                    }
                                    Mount::Node(node) => {
                                        if let Ok(rpcmsg) = frame.to_rpcmesage() {
                                            let mut rpcmsg2 = rpcmsg;
                                            rpcmsg2.set_shvpath(node_path);
                                            Some(node.process_request(&rpcmsg2))
                                        } else {
                                            Some(Err(RpcError::new(RpcErrorCode::InvalidRequest, &format!("Cannot convert RPC frame to Rpc message"))))
                                        }
                                    }
                                }
                            } else {
                                if let Ok(rpcmsg) = frame.to_rpcmesage() {
                                    Some(dir_ls(&broker.mounts, rpcmsg))
                                } else {
                                    Some(Err(RpcError::new(RpcErrorCode::InvalidRequest, &format!("Cannot convert RPC frame to Rpc message"))))
                                }
                            };
                            if let Some(result) = result {
                                if let Ok(meta) = response_meta {
                                    let mut resp = RpcMessage::from_meta(meta);
                                    match result {
                                        Ok((value, _signal)) => {
                                            resp.set_result(value);
                                        }
                                        Err(err) => {
                                            resp.set_error(err);
                                        }
                                    }
                                    let peer = broker.peers.get(&client_id).unwrap();
                                    peer.sender.send(PeerEvent::Message(resp)).await.unwrap();
                                }
                            }
                        } else if frame.is_response() {
                            if let Some(client_id) = frame.pop_caller_id() {
                                let peer = broker.peers.get(&client_id).unwrap();
                                peer.sender.send(PeerEvent::Frame(frame)).await.unwrap();
                            }
                        }
                    }
                    ClientEvent::NewClient { client_id, sender } => match broker.peers.entry(client_id) {
                        Entry::Occupied(..) => (),
                        Entry::Vacant(entry) => {
                            entry.insert(Peer {
                                sender,
                            });
                        }
                    },
                    ClientEvent::RegiterDevice { client_id, device_id, mount_point } => {
                        broker.mount_device(client_id, device_id, mount_point);
                    },
                    ClientEvent::ClientGone { client_id } => {
                        broker.peers.remove(&client_id);
                        let mount = if let Some((path, _)) = broker.mounts.iter().find(|(_, v)| {
                            match v {
                                Mount::Device(dev) => {dev.client_id == client_id}
                                Mount::Node(_) => {false}
                            }
                        }) {
                            Some(path.clone())
                        } else {
                            None
                        };
                        if let Some(path) = mount {
                            info!("Client id: {} disconnected, unmounting path: '{}'", client_id, &path);
                            broker.mounts.remove(&path);
                        }
                    }
                    ClientEvent::GetPassword { client_id, user } => {
                        let shapwd = broker.sha_password(&user);
                        let peer = broker.peers.get(&client_id).unwrap();
                        peer.sender.send(PeerEvent::PasswordSha1(shapwd)).await.unwrap();

                    }
                }
            }
        }
    }
    //drop(peers);
}

fn spawn_and_log_error<F>(fut: F) -> task::JoinHandle<()>
    where
        F: Future<Output = Result<()>> + Send + 'static,
{
    task::spawn(async move {
        if let Err(e) = fut.await {
            eprintln!("{}", e)
        }
    })
}
