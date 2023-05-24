use anyhow::{anyhow, Result};
use serde_derive::{Deserialize, Serialize};
use std::{
    collections::{hash_map::Entry, HashMap},
    env,
    io::{Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    println,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

type UserId = String;

#[derive(Serialize, Deserialize, Clone, Debug)]
enum Message {
    Text(String),
    NewUser,
    Quit(UserId),
    ServerQuit,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Packet {
    from: UserId,
    to: UserId,
    content: Message,
}

impl Packet {
    fn new_text(text: &str) -> Self {
        Packet {
            from: "n/a".to_owned(),
            to: "n/a".to_owned(),
            content: Message::Text(text.to_owned()),
        }
    }
}

struct Connection {
    stream: TcpStream,
    inbox: Sender<Packet>,
    outbox: Option<Sender<Packet>>,
    handles: Vec<JoinHandle<()>>,
    user: Option<UserId>,
}

impl Connection {
    fn new(stream: &TcpStream, shared_inbox: &Sender<Packet>) -> Result<Connection> {
        let conn = Connection {
            stream: stream.try_clone()?,
            inbox: shared_inbox.clone(),
            outbox: None,
            handles: Vec::new(),
            user: None,
        };
        Ok(conn)
    }

    fn handshake(&mut self, name: Option<String>) -> Result<UserId> {
        if let Some(name) = name.as_ref() {
            // Client Protocol
            let mut sig_header = [0u8; 4];
            self.stream.read_exact(&mut sig_header)?;
            let header_length = u32::from_be_bytes(sig_header);
            let mut server_sig = vec![0u8; header_length as usize];
            self.stream.read_exact(&mut server_sig)?;
            println!("received server sig: {}", String::from_utf8(server_sig)?);

            let response_header = (name.len() as u32).to_be_bytes();
            self.stream.write_all(&response_header)?;
            self.stream.write_all(name.as_bytes())?;

            self.user = Some(name.clone());

            Ok(name.clone())
        } else {
            // Server protocol
            let welcome = "chat-room 1.0".as_bytes();
            let header = (welcome.len() as u32).to_be_bytes();
            self.stream.write_all(&header)?;
            self.stream.write_all(welcome)?;

            let mut header = [0u8; 4];
            self.stream.read_exact(&mut header).expect("read length");
            let response_length = u32::from_be_bytes(header);
            let mut response = vec![0u8; response_length as usize];
            self.stream.read_exact(&mut response)?;
            let id = String::from_utf8(response)?;
            Ok(id)
        }
    }

    fn start(&mut self) -> Result<()> {
        // TODO(@christophe) Error is handshake() failed

        // sender thread: pop messages from outbox, send to client stream
        let stream_write = self.stream.try_clone()?;
        let (outbox, client_outbox) = channel::<Packet>();
        self.outbox = Some(outbox);
        let handle = thread::spawn(move || loop {
            // TODO: quit thread cleanly if stream or outbox disappear
            let stream = stream_write.try_clone().expect("clone stream");
            let message = client_outbox.recv().expect("client out queue");
            bincode::serialize_into(stream, &message).expect("serialization");
        });
        self.handles.push(handle);

        // receiver thread: read stream, push messages into room's inbox
        let room_inbox = self.inbox.clone();
        let stream_read = self.stream.try_clone()?;
        let user_id = self.user.clone();
        let handle = thread::spawn(move || loop {
            let stream = stream_read.try_clone().expect("clone stream");
            match bincode::deserialize_from(stream) {
                Ok(message) => {
                    println!("Got message: {:?}", message);
                    room_inbox.send(message).expect("sending msg");
                }
                Err(_) => {
                    let quit_messsage = if let Some(id) = user_id {
                        Packet {
                            from: id.clone(),
                            to: id.clone(),
                            content: Message::Quit(id.clone()),
                        }
                    } else {
                        Packet {
                            from: "".to_owned(),
                            to: "".to_owned(),
                            content: Message::ServerQuit,
                        }
                    };
                    println!("Disconnecting");
                    room_inbox.send(quit_messsage).expect("sending msg");
                    break;
                }
            }
        });
        self.handles.push(handle);

        Ok(())
    }

    fn send(&self, message: &Packet) {
        if let Some(outbox) = self.outbox.as_ref() {
            outbox.send(message.clone()).expect("Message not delivered");
        }
    }
}

struct Room {
    connections: Arc<Mutex<HashMap<UserId, Connection>>>,
    handles: Vec<JoinHandle<()>>, // XXX Maybe just Option
    inbox: Receiver<Packet>,
    local_id: Option<UserId>,
}

impl Room {
    fn create<A: ToSocketAddrs>(addr: A) -> Result<Room> {
        println!("Starting server");

        let (client_inbox, inbox) = channel::<Packet>();

        // server thread
        let server = TcpListener::bind(addr).expect("Bind");
        let clients = Arc::new(Mutex::new(HashMap::new()));
        let clients1 = clients.clone();
        let handle = thread::spawn(move || {
            for stream in server.incoming() {
                let stream = stream.expect("get stream");
                let mut conn = Connection::new(&stream, &client_inbox).expect("new connection");

                if let Ok(user_id) = conn.handshake(None) {
                    println!("New connection for {}", &user_id);
                    if conn.start().is_ok() {
                        if let Entry::Vacant(entry) = clients1
                            .lock()
                            .expect("could not lock client list")
                            .entry(user_id)
                        {
                            entry.insert(conn);
                        }
                    }
                }
            }
        });

        let mut handles = Vec::new();
        handles.push(handle);

        Ok(Room {
            connections: clients,
            handles,
            inbox,
            local_id: None,
        })
    }

    fn connect<A: ToSocketAddrs>(addr: A, user_id: UserId) -> Result<Room> {
        let addr = addr.to_socket_addrs()?.next().expect("Bad server address");
        println!("Trying to connect to server {:?}", addr);

        let stream = TcpStream::connect(addr)?;

        let (inbox_server, inbox) = channel();
        let mut server = Connection::new(&stream, &inbox_server)?;
        let local_id = server.handshake(Some(user_id))?;
        server.start()?;

        let mut connections = HashMap::new();
        connections.insert("server".to_owned(), server);

        Ok(Room {
            connections: Arc::new(Mutex::new(connections)),
            handles: Vec::new(),
            inbox,
            local_id: Some(local_id),
        })
    }

    fn send(&self, packet: Packet) -> Result<()> {
        if let Ok(connections) = self.connections.lock() {
            for (_, conn) in connections.iter() {
                conn.send(&packet);
            }
        }
        Ok(())
    }

    fn is_server(&self) -> bool {
        self.local_id.is_none()
    }

    fn update(&self) -> Result<()> {
        let message = self.inbox.recv()?;
        println!("Processing message {:?}", message);
        Ok(())
    }
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args().collect();

    if args.len() <= 1 {
        return Err(anyhow!("Not enough arguments"));
    }

    let room = if let Some(pos) =
        args.iter()
            .enumerate()
            .find_map(|(pos, arg)| if arg == "--client" { Some(pos) } else { None })
    {
        if pos < args.len() - 1 {
            let addr = &args[pos + 1];
            if let Some(pos) =
                args.iter()
                    .enumerate()
                    .find_map(|(pos, arg)| if arg == "--name" { Some(pos) } else { None })
            {
                if pos < args.len() - 1 {
                    let name = &args[pos + 1];
                    Room::connect(addr, name.to_string())
                } else {
                    Err(anyhow!("--name requires an argument after"))
                }
            } else {
                Err(anyhow!("Missing --name argument"))
            }
        } else {
            Err(anyhow!("--client requires an argument after"))
        }
    } else if args.iter().find(|&arg| arg == "--server").is_some() {
        Room::create("127.0.0.1:1234")
    } else {
        Err(anyhow!("Missing --client or --server argument"))
    }?;

    if room.is_server() {
        thread::sleep(Duration::from_secs(5));

        for _i in 1..10 {
            room.send(Packet::new_text("Hello"))?;
        }
    }

    loop {
        room.update()?;
    }

    Ok(())
}
