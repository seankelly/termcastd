extern crate mio;

use mio::*;
use mio::tcp::TcpListener;


const CASTER: Token = Token(0);
const WATCHER: Token = Token(1);


struct Termcastd {
    listen_caster: NonBlock<TcpListener>,
    listen_watcher: NonBlock<TcpListener>,
    casters: Vec<NonBlock<TcpStream>>,
}


impl Handler for Termcastd {
    type Timeout = ();
    type Message = ();
}


fn main() {
    let caster_addr = "127.0.0.1:31337".parse().unwrap();
    let listen_caster = tcp::listen(&caster_addr).unwrap();

    let watcher_addr = "127.0.0.1:2300".parse().unwrap();
    let listen_watcher = tcp::listen(&watcher_addr).unwrap();

    let mut event_loop = EventLoop::new().unwrap();
    event_loop.register(&listen_caster, CASTER).unwrap();
    event_loop.register(&listen_watcher, WATCHER).unwrap();

    let mut termcastd = Termcastd {
        listen_caster: listen_caster,
        listen_watcher: listen_watcher,
        casters: Vec::new(),
    };
    event_loop.run(&mut termcastd).unwrap();
}
