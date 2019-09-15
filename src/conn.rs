extern crate base64;

use mio::Poll;
use mio::Token;
use std::io::Write;
use std::result;
use std::str;

use crate::config;
use crate::stream::{Stream, StreamErr};
use crate::utils;
use crate::wire;
use crate::wire::{Cmd, Msg, Pfx};

pub(crate) struct Conn<'poll> {
    serv_addr: String,
    serv_port: u16,
    tls: bool,
    hostname: String,
    realname: String,

    /// Server password
    pass: Option<String>,

    nicks: Vec<String>,

    /// Always in range of `nicks`
    current_nick_idx: usize,

    /// Channels to auto-join. Every channel we join will be added here to be able to re-join
    /// automatically on reconnect and channels we leave will be removed.
    ///
    /// Technically a set but we want to join channels in the order given by the user, so using
    /// `Vec` here.
    auto_join: Vec<String>,

    /// Nickserv password. Sent to NickServ on connecting to the server and nick change, before
    /// join commands.
    nickserv_ident: Option<String>,

    /// Away reason if away mode is on. `None` otherwise.
    away_status: Option<String>,

    /// servername to be used in PING messages. Read from 002 RPL_YOURHOST.
    /// `None` until 002.
    servername: Option<String>,

    /// Our usermask given by the server. Currently only parsed after a JOIN,
    /// reply 396.
    ///
    /// Note that RPL_USERHOST (302) does not take cloaks into account, so we
    /// don't parse USERHOST responses to set this field.
    usermask: Option<String>,

    poll: &'poll Poll,

    status: ConnStatus<'poll>,

    /// Incoming message buffer
    in_buf: Vec<u8>,

    sasl_auth: Option<config::SASLAuth>,

    /// Do we have a nick yet? Try another nick on ERR_NICKNAMEINUSE (433) until we've got a nick.
    nick_accepted: bool,
}

pub(crate) type ConnErr = StreamErr;

/// How many ticks to wait before sending a ping to the server.
const PING_TICKS: u8 = 60;
/// How many ticks to wait after sending a ping to the server to consider a
/// disconnect.
const PONG_TICKS: u8 = 60;
/// How many ticks to wait after a disconnect or a socket error.
pub(crate) const RECONNECT_TICKS: u8 = 30;

enum ConnStatus<'poll> {
    PingPong {
        /// Ticks passed since last time we've heard from the server. Reset on
        /// each message. After `PING_TICKS` ticks we send a PING message and
        /// move to `WaitPong` state.
        ticks_passed: u8,
        stream: Stream<'poll>,
    },
    WaitPong {
        /// Ticks passed since we sent a PING to the server. After a message
        /// move to `PingPong` state. On timeout we reset the connection.
        ticks_passed: u8,
        stream: Stream<'poll>,
    },
    Disconnected {
        ticks_passed: u8,
    },
}

macro_rules! update_status {
    ($self:ident, $v:ident, $code:expr) => {{
        // temporarily putting `Disconnected` to `self.status`
        let $v = ::std::mem::replace(
            &mut $self.status,
            ConnStatus::Disconnected { ticks_passed: 0 },
        );
        let new_status = $code;
        $self.status = new_status;
    }};
}

impl<'poll> ConnStatus<'poll> {
    fn get_stream(&self) -> Option<&Stream<'poll>> {
        use self::ConnStatus::*;
        match *self {
            PingPong { ref stream, .. } | WaitPong { ref stream, .. } => Some(stream),
            Disconnected { .. } => None,
        }
    }

    fn get_stream_mut(&mut self) -> Option<&mut Stream<'poll>> {
        use self::ConnStatus::*;
        match *self {
            PingPong { ref mut stream, .. } | WaitPong { ref mut stream, .. } => Some(stream),
            Disconnected { .. } => None,
        }
    }
}

pub(crate) type Result<T> = result::Result<T, StreamErr>;

#[derive(Debug)]
pub(crate) enum ConnEv {
    /// Connected to the server + registered
    Connected,
    ///
    Disconnected,
    /// Hack to return the main loop that the Conn wants reconnect()
    WantReconnect,
    /// Network error happened
    Err(StreamErr),
    /// An incoming message
    Msg(Msg),
    /// Nick changed
    NickChange(String),
}

fn introduce<W: Write>(
    stream: &mut W,
    pass: Option<&str>,
    hostname: &str,
    realname: &str,
    nick: &str,
) {
    if let Some(pass) = pass {
        wire::pass(stream, pass).unwrap();
    }
    wire::nick(stream, nick).unwrap();
    wire::user(stream, hostname, realname).unwrap();
}

impl<'poll> Conn<'poll> {
    pub(crate) fn new(server: config::Server, poll: &'poll Poll) -> Result<Conn<'poll>> {
        let mut stream =
            Stream::new(poll, &server.addr, server.port, server.tls).map_err(StreamErr::from)?;

        if server.sasl_auth.is_some() {
            // Will introduce self after getting a response to this LS command.
            // This is to avoid getting stuck during nick registration. See the
            // discussion in #91.
            wire::cap_ls(&mut stream).unwrap();
        } else {
            introduce(
                &mut stream,
                server.pass.as_ref().map(String::as_str),
                &server.hostname,
                &server.realname,
                &server.nicks[0],
            );
        }

        Ok(Conn {
            serv_addr: server.addr,
            serv_port: server.port,
            tls: server.tls,
            hostname: server.hostname,
            realname: server.realname,
            pass: server.pass,
            nicks: server.nicks,
            current_nick_idx: 0,
            auto_join: server.join,
            nickserv_ident: server.nickserv_ident,
            away_status: None,
            servername: None,
            usermask: None,
            poll,
            status: ConnStatus::PingPong {
                ticks_passed: 0,
                stream,
            },
            in_buf: vec![],
            sasl_auth: server.sasl_auth,
            nick_accepted: false,
        })
    }

    pub(crate) fn reconnect(&mut self, new_serv: Option<(&str, u16)>) -> Result<()> {
        // drop existing connection first
        let old_stream = ::std::mem::replace(
            &mut self.status,
            ConnStatus::Disconnected { ticks_passed: 0 },
        );
        drop(old_stream);

        self.nick_accepted = false;

        if let Some((new_name, new_port)) = new_serv {
            self.serv_addr = new_name.to_owned();
            self.serv_port = new_port;
        }
        match Stream::new(self.poll, &self.serv_addr, self.serv_port, self.tls) {
            Err(err) => Err(err),
            Ok(mut stream) => {
                if self.sasl_auth.is_some() {
                    wire::cap_ls(&mut stream).unwrap();
                } else {
                    introduce(
                        &mut stream,
                        self.pass.as_ref().map(String::as_str),
                        &self.hostname,
                        &self.realname,
                        self.get_nick(),
                    );
                }
                self.status = ConnStatus::PingPong {
                    ticks_passed: 0,
                    stream,
                };
                self.current_nick_idx = 0;
                Ok(())
            }
        }
    }

    pub(crate) fn get_conn_tok(&self) -> Option<Token> {
        self.status.get_stream().map(|s| s.get_tok())
    }

    pub(crate) fn get_serv_name(&self) -> &str {
        &self.serv_addr
    }

    pub(crate) fn get_nick(&self) -> &str {
        &self.nicks[self.current_nick_idx]
    }

    pub(crate) fn is_nick_accepted(&self) -> bool {
        self.nick_accepted
    }

    /// Update the current nick state. Only do this after a new nick has given/accepted by the
    /// server.
    fn set_nick(&mut self, nick: &str) {
        if let Some(nick_idx) = self.nicks.iter().position(|n| n == nick) {
            self.current_nick_idx = nick_idx;
        } else {
            self.nicks.push(nick.to_owned());
            self.current_nick_idx = self.nicks.len() - 1;
        }
    }

    fn next_nick(&mut self) {
        if self.current_nick_idx + 1 == self.nicks.len() {
            let mut new_nick = self.nicks.last().unwrap().to_string();
            new_nick.push('_');
            self.status.get_stream_mut().map(|stream| {
                wire::nick(stream, &new_nick).unwrap();
            });
            self.nicks.push(new_nick);
        }
        self.current_nick_idx += 1;
    }
}

impl<'poll> Conn<'poll> {
    fn plain_sasl_authenticate(&mut self) {
        if let (Some(stream), Some(auth)) = (self.status.get_stream_mut(), self.sasl_auth.as_ref())
        {
            let msg = format!(
                "{}\x00{}\x00{}",
                auth.username, auth.username, auth.password
            );
            wire::authenticate(stream, &base64::encode(&msg)).unwrap();
        }
    }

    fn end_capability_negotiation(&mut self) {
        self.status.get_stream_mut().map(|stream| {
            wire::cap_end(stream).unwrap();
        });
    }

    pub(crate) fn enter_disconnect_state(&mut self) {
        self.status = ConnStatus::Disconnected { ticks_passed: 0 };
        self.nick_accepted = false;
    }

    ////////////////////////////////////////////////////////////////////////////
    // Tick handling

    pub(crate) fn tick(&mut self, evs: &mut Vec<ConnEv>) {
        update_status!(
            self,
            status,
            match status {
                ConnStatus::PingPong {
                    mut stream,
                    ticks_passed,
                } => {
                    let ticks = ticks_passed + 1;
                    if ticks == PING_TICKS {
                        match self.servername {
                            None => {
                                // debug_out.write_line(format_args!(
                                //     "{}: Can't send PING, servername unknown",
                                //     self.serv_addr
                                // ));
                            }
                            Some(ref host_) => {
                                wire::ping(&mut stream, host_).unwrap();
                            }
                        }
                        ConnStatus::WaitPong {
                            stream,
                            ticks_passed: 0,
                        }
                    } else {
                        ConnStatus::PingPong {
                            stream,
                            ticks_passed: ticks,
                        }
                    }
                }
                ConnStatus::WaitPong {
                    stream,
                    ticks_passed,
                } => {
                    let ticks = ticks_passed + 1;
                    if ticks == PONG_TICKS {
                        evs.push(ConnEv::Disconnected);
                        self.nick_accepted = false;
                        ConnStatus::Disconnected { ticks_passed: 0 }
                    } else {
                        ConnStatus::WaitPong {
                            stream,
                            ticks_passed: ticks,
                        }
                    }
                }
                ConnStatus::Disconnected { ticks_passed } => {
                    let ticks = ticks_passed + 1;
                    if ticks_passed + 1 == RECONNECT_TICKS {
                        // *sigh* it's slightly annoying that we can't reconnect here, we need to
                        // update the event loop
                        evs.push(ConnEv::WantReconnect);
                        self.current_nick_idx = 0;
                    }
                    ConnStatus::Disconnected {
                        ticks_passed: ticks,
                    }
                }
            }
        );
    }

    fn reset_ticks(&mut self) {
        update_status!(
            self,
            status,
            match status {
                ConnStatus::PingPong { stream, .. } => ConnStatus::PingPong {
                    ticks_passed: 0,
                    stream
                },
                ConnStatus::WaitPong { stream, .. } =>
                // no bug: we heard something from the server, whether it was a pong or not
                // doesn't matter that much, connectivity is fine.
                {
                    ConnStatus::PingPong {
                        ticks_passed: 0,
                        stream,
                    }
                }
                ConnStatus::Disconnected { .. } => status,
            }
        );
    }

    ////////////////////////////////////////////////////////////////////////////
    // Sending messages

    /// Send a nick message. Does not mean we will be successfully changing the nick, the new nick
    /// may be in use or for some other reason server may reject the request. Expect ERR_NICKINUSE
    /// or NICK message in response.
    pub(crate) fn send_nick(&mut self, nick: &str) {
        self.status.get_stream_mut().map(|stream| {
            wire::nick(stream, nick).unwrap();
        });
    }

    fn nickserv_ident(&mut self) {
        // FIXME: privmsg method inlined below to work around a borrowchk error
        if let Some(ref pwd) = self.nickserv_ident {
            self.status.get_stream_mut().map(|stream| {
                wire::privmsg(stream, "NickServ", &format!("identify {}", pwd)).unwrap();
            });
        }
    }

    /// `extra_len`: Size (in bytes) for a prefix/suffix etc. that'll be added to each line.
    /// Strings returned by the iterator will have enough room for that.
    pub(crate) fn split_privmsg<'a>(
        &self,
        extra_len: i32,
        msg: &'a str,
    ) -> utils::SplitIterator<'a> {
        // Max msg len calculation adapted from hexchat
        // (src/common/outbound.c:split_up_text)
        let mut max: i32 = 512; // RFC 2812
        max -= 3; // :, !, @
        max -= 13; // " PRIVMSG ", " ", :, \r, \n
        max -= self.get_nick().len() as i32;
        max -= extra_len;
        match self.usermask {
            None => {
                max -= 9; // max username
                max -= 64; // max possible hostname (63) + '@'
                           // NOTE(osa): I think hexchat has an error here, it
                           // uses 65
            }
            Some(ref usermask) => {
                max -= usermask.len() as i32;
            }
        }

        assert!(max > 0);

        utils::split_iterator(msg, max as usize)
    }

    // FIXME: This crashes with an assertion error when the message is too long
    // to fit into 512 bytes. Need to make sure `split_privmsg` is called before
    // this.
    pub(crate) fn privmsg(&mut self, target: &str, msg: &str) {
        self.status.get_stream_mut().map(|stream| {
            wire::privmsg(stream, target, msg).unwrap();
        });
    }

    pub(crate) fn ctcp_action(&mut self, target: &str, msg: &str) {
        self.status.get_stream_mut().map(|stream| {
            wire::ctcp_action(stream, target, msg).unwrap();
        });
    }

    pub(crate) fn join(&mut self, chans: &[&str]) {
        self.status.get_stream_mut().map(|stream| {
            wire::join(stream, chans).unwrap();
        });
        // the channel will be added to auto-join list on successful join (i.e.
        // after RPL_TOPIC)
    }

    pub(crate) fn part(&mut self, chan: &str) {
        self.status.get_stream_mut().map(|stream| {
            wire::part(stream, chan).unwrap();
        });
        self.auto_join.drain_filter(|chan_| chan_ == chan);
    }

    pub(crate) fn away(&mut self, msg: Option<&str>) {
        self.away_status = msg.map(|s| s.to_string());
        self.status.get_stream_mut().map(|stream| {
            wire::away(stream, msg).unwrap();
        });
    }

    pub(crate) fn raw_msg(&mut self, msg: &str) {
        self.status.get_stream_mut().map(|stream| {
            write!(stream, "{}\r\n", msg).unwrap();
        });
    }

    ////////////////////////////////////////////////////////////////////////////
    // Sending messages

    pub(crate) fn write_ready(&mut self, evs: &mut Vec<ConnEv>) {
        if let Some(stream) = self.status.get_stream_mut() {
            match stream.write_ready() {
                Err(err) => {
                    if !err.is_would_block() {
                        evs.push(ConnEv::Err(err));
                    }
                }
                Ok(()) => {}
            }
        }
    }

    ////////////////////////////////////////////////////////////////////////////
    // Receiving messages

    pub(crate) fn read_ready(&mut self, evs: &mut Vec<ConnEv>) {
        let mut read_buf: [u8; 512] = [0; 512];

        if let Some(stream) = self.status.get_stream_mut() {
            match stream.read_ready(&mut read_buf) {
                Err(err) => {
                    if !err.is_would_block() {
                        evs.push(ConnEv::Err(err));
                    }
                }
                Ok(bytes_read) => {
                    self.reset_ticks();
                    self.in_buf.extend(&read_buf[0..bytes_read]);
                    self.handle_msgs(evs);
                }
            }
        }
    }

    fn handle_msgs(&mut self, evs: &mut Vec<ConnEv>) {
        while let Some(msg) = Msg::read(&mut self.in_buf) {
            self.handle_msg(msg, evs);
        }
    }

    fn handle_msg(&mut self, msg: Msg, evs: &mut Vec<ConnEv>) {
        if let Msg {
            cmd:
                Cmd::CAP {
                    client: _,
                    ref subcommand,
                    ref params,
                },
            ..
        } = msg
        {
            match subcommand.as_ref() {
                "ACK" => {
                    if params.iter().any(|cap| cap.as_str() == "sasl") {
                        self.status.get_stream_mut().map(|stream| {
                            wire::authenticate(stream, "PLAIN").unwrap();
                        });
                    }
                }
                "NAK" => {
                    self.end_capability_negotiation();
                }
                "LS" => {
                    if let Some(stream) = self.status.get_stream_mut() {
                        introduce(stream, None, &self.hostname, &self.realname, &self.nicks[0]);
                        if params.iter().any(|cap| cap == "sasl") {
                            wire::cap_req(stream, &["sasl"]).unwrap();
                            // Will wait for CAP ... ACK from server before authentication.
                        }
                    }
                }
                _ => {}
            };
        }

        if let Msg {
            cmd: Cmd::AUTHENTICATE { ref param },
            ..
        } = msg
        {
            if param.as_str() == "+" {
                // Empty AUTHENTICATE response.  It means server accepted the specified SASL
                // mechanism (PLAIN)
                self.plain_sasl_authenticate();
            }
        }

        match msg.cmd {
            // 903: RPL_SASLSUCCESS, 904: ERR_SASLFAIL
            Cmd::Reply { num: 903, .. } | Cmd::Reply { num: 904, .. } => {
                self.end_capability_negotiation();
            }
            _ => {}
        }

        if let Msg {
            cmd: Cmd::PING { ref server },
            ..
        } = msg
        {
            self.status.get_stream_mut().map(|stream| {
                wire::pong(stream, server).unwrap();
            });
        }

        if let Msg {
            cmd: Cmd::JOIN { .. },
            pfx: Some(Pfx::User { ref nick, ref user }),
        } = msg
        {
            if nick == self.get_nick() {
                let usermask = format!("{}!{}", nick, user);
                self.usermask = Some(usermask);
            }
        }

        if let Msg {
            cmd: Cmd::Reply {
                num: 396,
                ref params,
            },
            ..
        } = msg
        {
            // :hobana.freenode.net 396 osa1 haskell/developer/osa1
            // :is now your hidden host (set by services.)
            if params.len() == 3 {
                let usermask = format!("{}!~{}@{}", self.get_nick(), self.hostname, params[1]);
                self.usermask = Some(usermask);
            }
        }

        if let Msg {
            cmd: Cmd::Reply {
                num: 302,
                ref params,
            },
            ..
        } = msg
        {
            // 302 RPL_USERHOST
            // :ircd.stealth.net 302 yournick :syrk=+syrk@millennium.stealth.net
            //
            // We know there will be only one nick because /userhost cmd sends
            // one parameter (our nick)
            //
            // Example args: ["osa1", "osa1=+omer@moz-s8a.9ac.93.91.IP "]

            let param = &params[1];
            match wire::find_byte(param.as_bytes(), b'=') {
                None => {
                    // logger
                    //     .get_debug_logs()
                    //     .write_line(format_args!("can't parse RPL_USERHOST: {}", params[1]));
                }
                Some(mut i) => {
                    if param.as_bytes().get(i + 1) == Some(&b'+')
                        || param.as_bytes().get(i + 1) == Some(&b'-')
                    {
                        i += 1;
                    }
                    let usermask = (&param[i..]).trim();
                    self.usermask = Some(usermask.to_owned());
                }
            }
        }

        if let Msg {
            cmd: Cmd::Reply { num: 001, .. },
            ..
        } = msg
        {
            // 001 RPL_WELCOME is how we understand that the registration was successful
            evs.push(ConnEv::Connected);
            evs.push(ConnEv::NickChange(self.get_nick().to_owned()));
            self.nickserv_ident();
            self.nick_accepted = true;
        }

        if let Msg {
            cmd: Cmd::Reply {
                num: 002,
                ref params,
            },
            ..
        } = msg
        {
            // 002    RPL_YOURHOST
            //        "Your host is <servername>, running version <ver>"

            // An example <servername>: cherryh.freenode.net[149.56.134.238/8001]

            match parse_servername(params) {
                None => {
                    // logger.get_debug_logs().write_line(format_args!(
                    //     "{} Can't parse hostname from params: {:?}",
                    //     self.serv_addr, params
                    // ));
                }
                Some(servername) => {
                    self.servername = Some(servername);
                }
            }
        }

        if let Msg {
            cmd: Cmd::Reply { num: 433, .. },
            ..
        } = msg
        {
            // ERR_NICKNAMEINUSE
            if !self.nick_accepted {
                self.next_nick();
            }
        }

        if let Msg {
            cmd: Cmd::NICK { nick: ref new_nick },
            pfx: Some(Pfx::User {
                nick: ref old_nick, ..
            }),
        } = msg
        {
            if old_nick == self.get_nick() {
                self.set_nick(new_nick);
                evs.push(ConnEv::NickChange(self.get_nick().to_owned()));
                self.nickserv_ident();
            }
        }

        if let Msg {
            cmd: Cmd::Reply { num: 376, .. },
            ..
        } = msg
        {
            if let Some(mut stream) = self.status.get_stream_mut() {
                // RPL_ENDOFMOTD. Join auto-join channels.
                if !self.auto_join.is_empty() {
                    wire::join(
                        &mut stream,
                        self.auto_join
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<&str>>()
                            .as_slice(),
                    )
                    .unwrap();
                }

                // Set away mode
                if let Some(ref reason) = self.away_status {
                    wire::away(stream, Some(reason)).unwrap();
                }
            }
        }

        if let Msg {
            cmd: Cmd::Reply {
                num: 332,
                ref params,
            },
            ..
        } = msg
        {
            if params.len() == 2 || params.len() == 3 {
                // RPL_TOPIC. We've successfully joined a channel, add the channel to
                // self.auto_join to be able to auto-join next time we connect
                let chan = &params[params.len() - 2];
                if !self.auto_join.contains(chan) {
                    self.auto_join.push(chan.to_owned());
                }
            }
        }

        evs.push(ConnEv::Msg(msg));
    }
}

/// Try to parse servername in a 002 RPL_YOURHOST reply
fn parse_servername(params: &[String]) -> Option<String> {
    let msg = params.get(1).or_else(|| params.get(0))?;
    let slice1 = &msg[13..];
    let servername_ends = wire::find_byte(slice1.as_bytes(), b'[')
        .or_else(|| wire::find_byte(slice1.as_bytes(), b','))?;
    Some((&slice1[..servername_ends]).to_owned())
}

////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_servername_1() {
        let args = vec![
            "tiny_test".to_owned(),
            "Your host is adams.freenode.net[94.125.182.252/8001], \
             running version ircd-seven-1.1.4"
                .to_owned(),
        ];
        assert_eq!(
            parse_servername(&args),
            Some("adams.freenode.net".to_owned())
        );
    }

    #[test]
    fn test_parse_servername_2() {
        let args = vec![
            "tiny_test".to_owned(),
            "Your host is belew.mozilla.org, running version InspIRCd-2.0".to_owned(),
        ];
        assert_eq!(
            parse_servername(&args),
            Some("belew.mozilla.org".to_owned())
        );
    }
}
