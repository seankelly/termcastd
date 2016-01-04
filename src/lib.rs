extern crate chrono;
extern crate mio;
#[macro_use]
extern crate log;

pub mod config;

mod auth;
mod ring;
mod term;

use chrono::{DateTime, Duration, UTC};
use mio::*;
use std::io::{Error, ErrorKind};
use std::io::Read;
use std::io::Write;
use mio::tcp::{TcpListener, TcpStream};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::net::SocketAddr;
use std::str;

use auth::CasterAuth;
use config::TermcastConfig;
use ring::RingBuffer;


const CASTER: Token = Token(0);
const WATCHER: Token = Token(1);
const CASTERS_PER_SCREEN: usize = 16;
const MENU_CHOICES: [&'static str; 16] = ["a", "b", "c", "d", "e", "f", "g",
                                          "h", "i", "j", "k", "l", "m", "n",
                                          "o", "p"];


struct Caster {
    sock: TcpStream,
    token: Token,
    name: Option<String>,
    cast_buffer: RingBuffer,
    watchers: Vec<WatcherLite>,
    connected: DateTime<UTC>,
    last_byte_received: DateTime<UTC>,
}

struct CasterMenuEntry {
    token: Token,
    name: String,
    num_watchers: usize,
    connected: DateTime<UTC>,
    last_byte_received: DateTime<UTC>,
}

struct MenuView {
    caster_entries: Vec<CasterMenuEntry>,
    total_watchers: usize,
}

struct Watcher {
    offset: usize,
    sock: TcpStream,
    input_buffer: [u8; 128],
    token: Token,
    state: WatcherState,
}

struct WatcherLite {
    sock: TcpStream,
    token: Token,
}

struct Termcastd {
    listen_caster: TcpListener,
    listen_watcher: TcpListener,
    clients: HashMap<Token, Client>,
    watchers: HashMap<Token, Watcher>,
    casters: HashMap<Token, Caster>,
    caster_auth: CasterAuth,
    next_token_id: usize,
    number_watching: u32,
    number_casting: u32,
}

pub struct TermcastServer {
    termcastd: Termcastd,
    config: TermcastConfig,
    event_loop: EventLoop<Termcastd>,
}


pub enum TermcastdMessage {
    CasterDisconnected(Token),
    WatcherDisconnected(Token),
    Quit,
}

enum AuthResults {
    InvalidLogin,
    InvalidName,
    MissingHello,
    NotEnoughParts,
    TooLong,
    TryAgain,
    Utf8Error,
}

#[derive(Clone, Copy, Debug)]
enum Client {
    Caster,
    Watcher,
}

enum WatcherState {
    Connecting,
    Disconnecting,
    MainMenu,
    Watching,
}

enum WatcherAction {
    Exit,
    Nothing,
    ShowMenu(usize),
    StopWatching,
    Watch(usize),
}


impl MenuView {
    fn render(&self, offset: usize) -> (String, Option<usize>) {
        fn caster_menu_entry(now: &DateTime<UTC>, choice: &'static str,
                             caster: &CasterMenuEntry) -> String {
            format!(" {}) {} (idle ???, connected {}, {} watching)\r\n",
                    choice, caster.name,
                    caster.connected.format("%F %T"),
                    caster.num_watchers)
        }

        let num_casters = self.caster_entries.len();
        // If the offset is too high, reset it to the last page.
        let actual_offset = if offset < num_casters {
            offset
        }
        else {
            let page_length = MENU_CHOICES.len();
            let pages = num_casters / page_length;
            if num_casters == 0 || num_casters % page_length != 0 {
                pages * page_length
            }
            else {
                (pages - 1) * page_length
            }
        };

        let menu_header = format!(
            concat!(
                "{}{}",
                "\r\n",
                " ## Termcast\r\n",
                " ## {} sessions available. {} watchers connected.\r\n\r\n",
            ),
            term::clear_screen(), term::reset_cursor(),
            num_casters, self.total_watchers);

        let mut menu = String::with_capacity(80*24);
        menu.push_str(&menu_header);

        let now = UTC::now();
        let caster_choices = self.caster_entries.iter()
                    .skip(offset)
                    .take(CASTERS_PER_SCREEN);
        for c in caster_choices.zip(MENU_CHOICES.iter()) {
            let (caster, choice) = c;
            menu.push_str(&caster_menu_entry(&now, choice, caster));
        }

        let menu_footer = concat!(
            "\r\n",
            "Watch which session? ('q' quits)",
            " ",
        );
        menu.push_str(&menu_footer);

        //return menu;
        if actual_offset != offset {
            return (menu, None);
        }
        else {
            return (menu, Some(actual_offset));
        }
    }
}

impl Watcher {
    fn parse_input(&mut self, menu_view: &MenuView) -> WatcherAction {
        while let Ok(num_bytes) = self.sock.read(&mut self.input_buffer) {
            let each_byte = 0..num_bytes;
            //let channel = event_loop.channel();
            for (_offset, byte) in each_byte.zip(self.input_buffer.iter()) {
                match self.state {
                    WatcherState::Watching => {
                        // Pressing 'q' while watching returns the watcher to the main menu.
                        if *byte == 113 {
                            // This will reset the state back to the main menu.
                            self.state = WatcherState::MainMenu;
                            return WatcherAction::StopWatching;
                        }
                    },
                    WatcherState::MainMenu => {
                        match *byte {
                            97...112 => { // a...p
                                // a = 97.
                                let page_offset = *byte as usize - 97;
                                // Check if the entry picked is still valid.
                                let caster_offset = self.offset + page_offset;
                                return WatcherAction::Watch(caster_offset);
                            }
                            113 => { // q
                                self.state = WatcherState::Disconnecting;
                                return WatcherAction::Exit;
                            },
                            // Any other character, refresh the menu.
                            _ => {
                                let (menu, fixed_offset) = menu_view.render(self.offset);
                                if let Some(offset) = fixed_offset {
                                    self.offset = offset;
                                }
                                self.sock.write(&menu.as_bytes());
                            },
                        }
                    },
                    WatcherState::Connecting => {},
                    WatcherState::Disconnecting => { return WatcherAction::Nothing },
                }
            }
        }

        return WatcherAction::Nothing;
    }

    fn send_menu(&mut self, menu_view: &MenuView) -> Result<usize, Error> {
        let (menu, fixed_offset) = menu_view.render(self.offset);
        if let Some(offset) = fixed_offset {
            self.offset = offset;
        }
        self.sock.write(&menu.as_bytes())
    }

    fn caster_copy(&mut self) -> Result<WatcherLite, Error> {
        let socket = try!(self.sock.try_clone());
        let lite = WatcherLite {
            sock: socket,
            token: self.token,
        };
        Ok(lite)
    }
}

impl Caster {
    fn input(&mut self, caster_auth: &mut CasterAuth) -> Result<(), ()> {
        let mut bytes_received = [0u8; 1024];
        while let Ok(_num_bytes) = self.sock.read(&mut bytes_received) {
            // If a name is set then all bytes go straight to the watchers.
            if self.name.is_some() {
                self.relay_input(&bytes_received);
            }
            else {
                let auth = self.handle_auth(&bytes_received, caster_auth);
                match auth {
                    Ok((offset, name)) => {
                        self.name = Some(name);
                        self.relay_input(&bytes_received[offset..]);
                    },
                    // Not enough data sent so try again later.
                    Err(AuthResults::TryAgain) => {},
                    Err(_) => return Err(()),
                }
            }
        }

        Ok(())
    }

    fn relay_input(&mut self, input: &[u8]) {
        for watcher in self.watchers.iter_mut() {
            let res = watcher.sock.write(&input);
            // Need to notify the watcher has an error.
            if res.is_err() {
            }
        }
    }

    // The very first bytes sent should be in utf-8:
    //   hello <name> <password>
    fn handle_auth(&mut self, raw_input: &[u8], caster_auth: &mut CasterAuth) -> Result<(usize, String), AuthResults> {
        // Limit the buffer used for the authentication to 1024 bytes. This is to limit a DoS and
        // reduce the possibility of getting into an unknown state.
        let mut auth_buffer = [0; 1024];

        if raw_input.len() + self.cast_buffer.len() > auth_buffer.len() {
            return Err(AuthResults::TooLong);
        }

        for el in self.cast_buffer.iter().enumerate() {
            let (idx, byte) = el;
            auth_buffer[idx] = byte;
        }

        let cb_len = self.cast_buffer.len();
        for el in raw_input.iter().enumerate() {
            let (idx, byte) = el;
            auth_buffer[idx+cb_len] = *byte;
        }

        // Try to find a newline as that marks the end of the opening message.
        if let Some(newline_idx) = auth_buffer.iter().position(|b| *b == 10) {
            if let Ok(input) = str::from_utf8(&auth_buffer[..newline_idx]) {
                let parts: Vec<&str> = input.splitn(3, ' ').collect();

                if parts.len() < 2 {
                    return Err(AuthResults::NotEnoughParts);
                }
                else if parts[0] != "hello" {
                    return Err(AuthResults::MissingHello);
                }

                let name = parts[1];
                // Valid names must have a length and consist of characters/bytes greater than 32.
                // The splitn above prevents spaces and this check verifies no control codes are in
                // the name.
                if name.len() == 0 {
                    return Err(AuthResults::InvalidName);
                }
                else if name.as_bytes().iter().any(|b| *b < 32) {
                    return Err(AuthResults::InvalidName);
                }
                // Allow the password field to be empty. Default to the empty string.
                let password = if parts.len() >= 3 { parts[2] } else { "" };
                // Would like to use this but can't get the types to quite work out.
                //let password = parts.get(2).unwrap_or("");
                if let Ok(_) = caster_auth.login(&name, &password) {
                    // Determine if there are any remaining bytes in raw_input. Reset the
                    // cast_buffer to contain those bytes.
                    self.cast_buffer.clear();
                    let cast_byte_idx = newline_idx + 1;
                    let offset = if cast_byte_idx < auth_buffer.len() {
                        self.cast_buffer.add(&auth_buffer[cast_byte_idx..]);
                        // Extra bytes left in the buffer. Need to return the index into raw_input
                        // so the calling function can relay those bytes.
                        cast_byte_idx - cb_len
                    }
                    else {
                        0
                    };
                    return Ok((offset, String::from(name)));
                }
                else {
                    return Err(AuthResults::InvalidLogin);
                }
            }
            else {
                return Err(AuthResults::Utf8Error);
            }
        }
        else {
            // No new line found so add all of the data to the ring buffer. Return an "error"
            // indicating not authenticated yet.
            let res = self.cast_buffer.add_no_wraparound(&raw_input);
            if res.is_err() {
                return Err(AuthResults::TooLong);
            }
            return Err(AuthResults::TryAgain);
        }
    }
}

impl Termcastd {
    fn new(listen_caster: TcpListener, listen_watcher: TcpListener) -> Self {
        Termcastd {
            listen_caster: listen_caster,
            listen_watcher: listen_watcher,
            clients: HashMap::new(),
            casters: HashMap::new(),
            caster_auth: CasterAuth::new(),
            watchers: HashMap::new(),
            next_token_id: 2,
            number_watching: 0,
            number_casting: 0,
        }
    }

    fn next_token(&mut self) -> Token {
        let token = Token(self.next_token_id);
        self.next_token_id += 1;
        return token;
    }

    fn menu_view(&self) -> MenuView {
        let valid_casters = self.casters.values()
            .filter(|c| c.name.is_some())
            .map(|c| {
                CasterMenuEntry {
                    token: c.token,
                    name: c.name.as_ref().unwrap().clone(),
                    num_watchers: c.watchers.len(),
                    connected: c.connected,
                    last_byte_received: c.last_byte_received,
                }
            });

        let view = MenuView {
            caster_entries: valid_casters.collect(),
            total_watchers: self.watchers.len(),
        };
        return view;
    }

    // Section for Caster and Watcher functions.
    ////////////////////////////////////
    fn handle_disconnect(&mut self, event_loop: &mut EventLoop<Termcastd>, token: Token) {
        if let Entry::Occupied(client) = self.clients.entry(token) {
            match client.get() {
                &Client::Caster => {
                    if let Entry::Occupied(caster_entry) = self.casters.entry(token) {
                        {
                            let caster = caster_entry.get();
                            let res = event_loop.deregister(&caster.sock);
                            self.number_casting -= 1;
                            let channel = event_loop.channel();
                            // To not have to do a mutable borrow, send a message to
                            // reset these watchers back to the main menu. Everything
                            // will be dropped after the end of the match when the
                            // entry is removed.
                            for watcher in caster.watchers.iter() {
                                let res = channel.send(TermcastdMessage::CasterDisconnected(watcher.token));
                            }
                        }
                        caster_entry.remove();
                    }
                },
                &Client::Watcher => {
                    if let Entry::Occupied(watcher_entry) = self.watchers.entry(token) {
                        {
                            let watcher = watcher_entry.get();
                            let res = event_loop.deregister(&watcher.sock);
                        }
                        self.number_watching -= 1;
                        watcher_entry.remove();
                    }
                },
            }
            client.remove();
        }
        else {
            panic!("Couldn't find token {:?} in self.clients", token);
        }
    }

    // Section for Caster functions.
    ////////////////////////////////////
    fn new_caster(&mut self, event_loop: &mut EventLoop<Termcastd>) {
        if let Ok(opt) = self.listen_caster.accept() {
            if let Some(sock) = opt {
                let token = self.next_token();
                let caster = Caster {
                    sock: sock,
                    token: token,
                    name: None,
                    cast_buffer: RingBuffer::new(90_000),
                    watchers: Vec::new(),
                    connected: UTC::now(),
                    last_byte_received: UTC::now(),
                };
                let res = event_loop.register_opt(
                    &caster.sock,
                    token,
                    EventSet::all(),
                    PollOpt::edge(),
                );
                if res.is_ok() {
                    self.number_casting += 1;
                    let client = Client::Caster;
                    self.clients.insert(token, client);
                    self.casters.insert(token, caster);
                }
            }
        }
    }

    /// Wrapper function for handling caster input. The caster will be removed if an error is
    /// returned from the wrapped function. Any watchers of that caster will be reset back to the
    /// main menu.
    fn read_caster(&mut self, event_loop: &mut EventLoop<Termcastd>, token: Token) {
        if let Err(watchers) = self.caster_input(event_loop, token) {
            for watcher in watchers.iter() {
                self.reset_watcher(event_loop, *watcher);
            }

            let _ = self.casters.remove(&token);
        }
    }

    /// Actual method to interface between the Caster method to parse the input and Termcastd. If
    /// there is an error, then the watchers for that caster will be grouped together and sent up
    /// the call chain to have them reset.
    fn caster_input(&mut self, event_loop: &mut EventLoop<Termcastd>, token: Token) -> Result<(), Vec<Token>> {
        if let Some(caster) = self.casters.get_mut(&token) {
            if let Err(_) = caster.input(&mut self.caster_auth) {
                // Assemble all of the watchers, stuff them in a vector, and send it up to have
                // those watchers reset to the main menu.
                let watchers = caster.watchers.iter()
                    .map(|w| w.token).collect();

                event_loop.deregister(&caster.sock);

                return Err(watchers);
            }
        }
        else {
            // Got an event for a token with no matching socket.
            return Err(Vec::new());
        }
        Ok(())
    }

    // Section for Watcher functions.
    ////////////////////////////////////
    fn new_watcher(&mut self, event_loop: &mut EventLoop<Termcastd>) -> Result<(), Error> {
        match self.listen_watcher.accept() {
            Ok(Some(sock)) => {
                let token = self.next_token();
                let watcher = Watcher {
                    offset: 0,
                    sock: sock,
                    input_buffer: [0; 128],
                    token: token,
                    state: WatcherState::Connecting,
                };

                try!(event_loop.register_opt(
                    &watcher.sock,
                    token,
                    EventSet::all(),
                    PollOpt::edge(),
                ));

                self.number_watching += 1;

                let offset = watcher.offset;
                self.clients.insert(token, Client::Watcher);
                self.watchers.insert(token, watcher);

                let menu_view = self.menu_view();

                let watcher_init = self.watchers.get_mut(&token)
                    .ok_or(Error::new(ErrorKind::NotFound, ""))
                    .and_then(|w| {
                        w.sock.write(&term::disable_linemode())
                            .map_err(|_err| event_loop.deregister(&w.sock))
                            .map_err(|_| Error::new(ErrorKind::Other, ""))
                            .map(|_| w)
                    })
                    .and_then(|w| {
                        w.sock.write(&term::disable_local_echo())
                            .map_err(|_err| event_loop.deregister(&w.sock))
                            .map_err(|_| Error::new(ErrorKind::Other, ""))
                            .map(|_| w)
                    })
                    .and_then(|w| {
                        w.state = WatcherState::MainMenu;
                        w.send_menu(&menu_view)
                            .map_err(|_err| event_loop.deregister(&w.sock))
                            .map_err(|_| Error::new(ErrorKind::Other, ""))
                            .map(|_| w)
                    });

                // Success does not need to return anything.
                watcher_init.map(|_| ())
            }
            Ok(None) => {
                Ok(())
            },
            Err(err) => {
                Err(err)
            },
        }
    }

    /// Wrapper function for when the casters structure needs to be modified.
    fn read_watcher(&mut self, event_loop: &mut EventLoop<Termcastd>, token: Token) {
        match self.watcher_input(event_loop, token) {
            Ok(WatcherAction::Exit) => {
                let _ = self.watchers.remove(&token);
            },
            Ok(_) => {},
            Err(_) => {}
        }
    }

    /// Handle actions affecting the watcher in this method. Actions that affect casters will be
    /// sent up the call chain.
    fn watcher_input(&mut self, event_loop: &mut EventLoop<Termcastd>, token: Token) -> Result<WatcherAction, ()> {
        let menu_view = self.menu_view();
        if let Some(mut watcher) = self.watchers.get_mut(&token) {
            loop {
                match watcher.parse_input(&menu_view) {
                    // The watcher returns the overall offset. Check that offset points to a valid
                    // caster. If it does, move the watcher to watch that caster. If it does not,
                    // refresh the menu for that watcher.
                    WatcherAction::Watch(offset) => {},
                    WatcherAction::StopWatching => { },
                    WatcherAction::ShowMenu(offset) => {},
                    WatcherAction::Exit => {
                        return Ok(WatcherAction::Exit);
                    },
                    WatcherAction::Nothing => { break },
                }
            }
        }
        else {
            // Got an event for a token with no matching socket.
        }

        Ok(WatcherAction::Nothing)
    }

    fn watcher_output(&mut self, token: Token, output: &[u8]) -> Option<Error> {
        self.watchers.get_mut(&token)
                     .and_then(|w| w.sock.write(&output).err())
    }

    fn reset_watcher(&mut self, event_loop: &mut EventLoop<Termcastd>, token: Token) {
        let menu_view = self.menu_view();
        self.watchers.get_mut(&token)
                     .and_then(|w| {
                         w.state = WatcherState::MainMenu;
                         w.send_menu(&menu_view).err()
                     });
    }
}

impl Handler for Termcastd {
    type Timeout = ();
    type Message = TermcastdMessage;

    fn ready(&mut self, event_loop: &mut EventLoop<Termcastd>, token: Token, event: EventSet) {
        match token {
            CASTER => {
                self.new_caster(event_loop);
            },
            WATCHER => {
                self.new_watcher(event_loop);
            },
            _ => {
                let client = {
                    *self.clients.get(&token).expect("Expected to find token.")
                };
                match (event.is_readable(), event.is_hup(), event.is_error(), client) {
                    (true, false, false, Client::Caster) => {
                        self.read_caster(event_loop, token);
                    },
                    (true, false, false, Client::Watcher) => {
                        self.read_watcher(event_loop, token);
                    },
                    (_, true, false, _) => {
                        self.handle_disconnect(event_loop, token);
                    },
                    (_, _, true, _) => {},
                    (false, false, false, _) => {},
                };
            },
        }
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Termcastd>, message: TermcastdMessage) {
        match message {
            TermcastdMessage::CasterDisconnected(token) => {
            },
            TermcastdMessage::WatcherDisconnected(token) => {
                self.handle_disconnect(event_loop, token);
            },
            TermcastdMessage::Quit => {
                event_loop.shutdown();
            }
        }
    }
}


impl TermcastServer {
    pub fn new(config: TermcastConfig) -> Result<Self, Error> {
        let listen_caster = try!(TcpListener::bind(&config.caster));
        let listen_watcher = try!(TcpListener::bind(&config.watcher));
        let termcastd = Termcastd::new(listen_caster, listen_watcher);
        let mut event_loop = EventLoop::new().unwrap();
        event_loop.register(&termcastd.listen_caster, CASTER).unwrap();
        event_loop.register(&termcastd.listen_watcher, WATCHER).unwrap();

        Ok(TermcastServer {
            termcastd: termcastd,
            config: config,
            event_loop: event_loop,
        })
    }

    pub fn run(&mut self) {
        self.event_loop.run(&mut self.termcastd).unwrap();
    }

    pub fn get_channel(&mut self) -> Sender<TermcastdMessage> {
        self.event_loop.channel()
    }

    pub fn get_socket_addrs(&self) -> Result<(SocketAddr, SocketAddr), Error> {
        let caster_addr = try!(self.termcastd.listen_caster.local_addr());
        let watcher_addr = try!(self.termcastd.listen_watcher.local_addr());
        Ok((caster_addr, watcher_addr))
    }
}
