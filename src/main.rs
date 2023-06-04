use anyhow::{anyhow, Result};
use crossterm::{
    cursor::{self, MoveToNextLine},
    event::{self, Event, KeyCode},
    execute, queue,
    style::{self, Attribute, Print, SetAttribute},
    terminal::{self, ClearType},
    QueueableCommand,
};
use serde_derive::{Deserialize, Serialize};
use std::{
    cell::Cell,
    collections::{hash_map::Entry, HashMap},
    env,
    fmt::Display,
    io::{stdout, Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    println, process,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
};

type UserId = String;

#[derive(Serialize, Deserialize, Clone, Debug)]
enum Message {
    Text(String),
    NewUser,
    Quit(UserId),
    ServerQuit,
    PromptUpdate,
    PromptSend(String),
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

    fn new_prompt_update() -> Self {
        Packet {
            from: "".to_owned(),
            to: "".to_owned(),
            content: Message::PromptUpdate,
        }
    }

    fn new_prompt_send(text: &str) -> Self {
        Packet {
            from: "".to_owned(),
            to: "".to_owned(),
            content: Message::PromptSend(text.to_owned()),
        }
    }
}

fn print(packet: &Packet) -> Result<()> {
    let mut s = stdout();
    match &packet.content {
        Message::NewUser => {
            queue!(
                s,
                Print(" |?| User "),
                SetAttribute(Attribute::Bold),
                Print("["),
                Print("unknowned"),
                Print("]"),
                SetAttribute(Attribute::Reset),
                Print(" has joined the room."),
                MoveToNextLine(1)
            )?;
        }
        Message::Text(text) => {
            queue!(
                s,
                SetAttribute(Attribute::Bold),
                Print("["),
                Print(&packet.from),
                Print("] "),
                SetAttribute(Attribute::Reset)
            )?;
            let padding = String::from_utf8(vec![' ' as u8; &packet.from.len() + 3]).unwrap();
            text.split('\n').enumerate().for_each(|(num, line)| {
                if num > 0 {
                    queue!(s, Print(&padding)).expect("");
                }
                queue!(
                    s,
                    Print(line),
                    terminal::Clear(ClearType::UntilNewLine),
                    MoveToNextLine(1)
                )
                .expect("");
            });
        }
        _ => {}
    }
    Ok(())
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
            //println!("received server sig: {}", String::from_utf8(server_sig)?);

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
    inbox_post: Sender<Packet>,
    local_id: Option<UserId>,
    history: Vec<Packet>,
}

impl Room {
    fn create<A: ToSocketAddrs>(addr: A) -> Result<Room> {
        println!("Starting server");

        let (client_inbox, inbox) = channel::<Packet>();
        let inbox_post = client_inbox.clone();

        // server thread
        let server = TcpListener::bind(addr).expect("Bind");
        let clients = Arc::new(Mutex::new(HashMap::new()));
        let clients1 = clients.clone();
        let handle = thread::spawn(move || {
            for stream in server.incoming() {
                let stream = stream.expect("get stream");
                let mut conn = Connection::new(&stream, &client_inbox).expect("new connection");

                if let Ok(user_id) = conn.handshake(None) {
                    //println!("New connection for {}", &user_id);
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
            inbox_post,
            local_id: None,
            history: Vec::new(),
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
            inbox_post: inbox_server,
            local_id: Some(local_id),
            history: Vec::new(),
        })
    }

    fn send(&self, packet: Packet) -> Result<()> {
        if let Ok(connections) = self.connections.lock() {
            for (_, conn) in connections.iter() {
                // println!(
                //     "from= {:?}, local_id: {:?}",
                //     &packet.from,
                //     self.local_id.as_ref().unwrap_or(&"server".to_owned())
                // );
                //if &packet.from != self.local_id.as_ref().unwrap_or(&"server".to_owned()) {
                conn.send(&packet);
                //}
            }
        }
        Ok(())
    }

    fn is_server(&self) -> bool {
        self.local_id.is_none()
    }

    fn update(&mut self) -> Result<()> {
        let mut s = stdout();

        if let Some(message) = self.inbox.recv().ok() {
            match &message.content {
                Message::NewUser => {
                    print(&message)?;
                }
                Message::Text(_) => {
                    print(&message)?;
                    s.flush()?;

                    // server forwards message to other clients
                    if self.is_server() {
                        self.send(message.clone())?;
                        self.history.push(message.clone());
                    }
                }
                Message::PromptSend(text) => {
                    let packet = Packet {
                        from: self
                            .local_id
                            .as_ref()
                            .unwrap_or(&String::from("server"))
                            .to_owned(),
                        to: "".to_owned(),
                        content: Message::Text(text.clone()),
                    };
                    if self.is_server() {
                        print(&packet)?;
                        s.flush()?;
                        self.history.push(packet.clone());
                    }

                    self.send(packet)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

struct Prompt {
    text: Vec<String>,
    cursor: (u16, u16),
}

impl Prompt {
    fn new() -> Prompt {
        Prompt {
            text: vec!["".to_owned()],
            cursor: (0, 0),
        }
    }

    fn clear(&mut self) {
        self.text.clear();
        self.text.push("".to_owned());
        self.cursor = (0, 0);
    }

    fn text(&self) -> String {
        self.text
            .iter()
            .fold(String::new(), |str, line| str + line + "\n")
            .trim_end()
            .into()
    }

    fn print(&self) {
        let mut s = stdout();
        let sep = ['—'; 30];
        queue!(
            s,
            cursor::MoveToColumn(0),
            style::Print(sep.iter().collect::<String>()),
            terminal::Clear(ClearType::UntilNewLine),
            style::Print("\n"),
            cursor::MoveToColumn(0)
        )
        .expect("");
        for (num, line) in self.text.iter().enumerate() {
            if num == 0 {
                s.queue(style::Print("message ")).expect("");
            } else {
                s.queue(style::Print("        ")).expect("");
            }
            queue!(
                s,
                style::Print("|"),
                style::Print(line),
                terminal::Clear(ClearType::UntilNewLine),
                style::Print("\n"),
                cursor::MoveToColumn(0)
            )
            .expect("");
        }
        queue!(s, terminal::Clear(ClearType::FromCursorDown)).expect("");

        // Move terminal cursor to prompt cursor
        let cursor = self.cursor();
        if self.text.len() > 0 {
            queue!(
                s,
                cursor::MoveToPreviousLine(self.text.len() as u16 - cursor.1)
            )
            .expect("");
        }
        queue!(s, cursor::MoveToColumn(9 + cursor.0)).expect("");

        s.flush().expect("");
    }

    fn rewind(&self) {
        let mut s = stdout();
        s.queue(cursor::MoveToPreviousLine(self.cursor.1 + 1 as u16))
            .expect("");
    }

    fn move_cursor_left(&mut self) {
        if self.cursor().0 == 0 && self.cursor.1 > 0 {
            self.cursor.1 = self.cursor.1.saturating_sub(1);
            self.cursor.0 = self.text[self.cursor.1 as usize].len() as u16;
        } else {
            self.cursor.0 = self.cursor().0.saturating_sub(1);
        }
    }

    fn move_cursor_right(&mut self) {
        let line_len = self.text[self.cursor.1 as usize].len() as u16;
        let num_lines = self.text.len() as u16;
        if self.cursor().0 == line_len && self.cursor.1 < num_lines - 1 {
            self.cursor.1 += 1;
            self.cursor.0 = 0;
        } else {
            self.cursor.0 += 1;
        }
    }

    fn move_cursor_up(&mut self) {
        self.cursor.1 = self.cursor.1.saturating_sub(1);
    }

    fn move_cursor_down(&mut self) {
        self.cursor.1 = (self.cursor.1 + 1).min(self.text.len() as u16 - 1);
    }

    fn put_char(&mut self, ch: char) {
        if ch.len_utf8() == 1 {
            let cursor = self.cursor();
            self.text[cursor.1 as usize].insert(cursor.0 as usize, ch);
            self.cursor = (cursor.0 + 1 as u16, cursor.1);
        }
    }

    fn backspace(&mut self) {
        if self.cursor().0 == 0 {
            if self.cursor.1 > 0 {
                let cur_vpos = self.cursor.1 as usize;
                let new_vpos = cur_vpos - 1;
                let new_hpos = self.text[new_vpos].len() as u16;
                let cur_line = self.text[cur_vpos].clone();
                self.text[new_vpos].push_str(&cur_line);
                self.text.remove(cur_vpos);
                self.cursor = (new_hpos, new_vpos as u16);
            }
        } else {
            let pos = self.cursor().0 as usize - 1;
            self.text[self.cursor.1 as usize].remove(pos);
            self.cursor.0 -= 1;
        }
    }

    fn newline(&mut self) {
        let split_pos = self.cursor().0 as usize;
        let new_line = self.text[self.cursor.1 as usize].split_off(split_pos);
        self.text.insert(self.cursor.1 as usize + 1, new_line);
        self.cursor = (0, self.cursor.1 + 1);
    }

    fn cursor(&self) -> (u16, u16) {
        let line_len = self.text[self.cursor.1 as usize].len() as u16;
        (self.cursor.0.min(line_len), self.cursor.1)
    }
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args().collect();

    if args.len() <= 1 {
        return Err(anyhow!("Not enough arguments"));
    }

    let mut room = if let Some(pos) =
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

    terminal::enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(
        stdout,
        event::PushKeyboardEnhancementFlags(
            event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    )?;

    let prompt = Arc::new(Mutex::new(Prompt::new()));
    let prompt2 = prompt.clone();

    let inbox = room.inbox_post.clone();
    thread::spawn(move || loop {
        let event = event::read().expect("read terminal");
        let mut prompt = prompt2.lock().expect("lock prompt");
        match event {
            Event::Key(key_event) => match key_event.code {
                KeyCode::Char(c) => {
                    prompt.put_char(c);
                }
                KeyCode::Tab /*if key_event.modifiers.contains(event::KeyModifiers::SHIFT)*/ => {
                    inbox
                        .send(Packet::new_prompt_send(&prompt.text()))
                        .expect("send prompt");
                    prompt.clear();
                }
                KeyCode::Enter => {
                    prompt.newline();
                }
                KeyCode::Backspace => {
                    prompt.backspace();
                }
                KeyCode::Up => {
                    prompt.move_cursor_up();
                }
                KeyCode::Down => {
                    prompt.move_cursor_down();
                }
                KeyCode::Right => {
                    prompt.move_cursor_right();
                }
                KeyCode::Left => {
                    prompt.move_cursor_left();
                }
                // KeyCode::Delete => {
                //     let mut prompt = prompt2.lock().expect("lock prompt");
                //     prompt.delete();
                // }
                KeyCode::Esc => {
                    terminal::disable_raw_mode().expect("");
                    execute!(stdout, event::PopKeyboardEnhancementFlags).expect("");
                    process::exit(1);
                }
                _ => (),
            },
            _ => {}
        }
        inbox
            .send(Packet::new_prompt_update())
            .expect("update prompt");
    });

    loop {
        if let Some(prompt) = prompt.lock().ok() {
            prompt.print();
            prompt.rewind();
        }

        room.update()?;
    }
    //Ok(())
}
