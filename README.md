git  test app for communication between a server and multiple clients over TCP. Uses [bincode/serde](https://docs.rs/bincode/latest/bincode/) to (de)serialize messages (from)to a `TcpStream`.

Used as investigation for adding multiplayer into a [tetris clone](https://github.com/goodartistscopy/ttrys).

Design
------
The program creates a `Room` which manages the connected instances. Communication with remote instances is handled with the `Connection` struct. In "server mode", the `Room` has one Connection per connected instance (client). In "client mode", the `Room` has a single `Connection` to the server. The program sends messages to the `Room` which either dispatches them to all clients (server mode) or sends them to the server (client mode).
In server-mode the room launches a thread to watch for incoming TCP connections. Each connections eventually spawns 2 threads, one which reads the `TcpStream` and pushes messages to the `Room`'s shared inbox. The other which pops messages from a local outbox and writes them to the `TcpStream`. These inbox/outbox message queues use `mpsc::channel()`.

TODO
----
* actually do something useful (probably a basic chat room)
* handle user id clashes properly
* handle errors in spawned threads properly
