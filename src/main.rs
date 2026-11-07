#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]
//! litecord GUI: a light Discord client (Bot API) with an egui interface.
//!
//! Backend (gateway / REST / image workers) is plain threads + channels, no async runtime.
//! The UI thread only repaints when an event arrives (a pump thread wakes egui).

use std::{
    collections::{HashMap, VecDeque},
    env,
    io::{self, Read},
    net::TcpStream,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

mod image;
mod oauth;
mod ui;
mod voice;
use image::Pic;

const API: &str = "https://discord.com/api/v10";
const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
// GUILDS | GUILD_MESSAGES | GUILD_MESSAGE_REACTIONS | MESSAGE_CONTENT (privileged: enable it in the developer portal)
const INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 10) | (1 << 15);
const MAX_MSGS: usize = 200;
const MAX_OLDER: usize = 500; // cap after loading older history
const UA: &str = "DiscordBot (https://example.invalid/litecord, 0.2.0)";

// ───────────────────────── data ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
struct ImgRef {
    name: String,
    url: String,
    w: u32,
    h: u32,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct Reaction {
    key: String,   // unicode emoji, or "name:id" for custom emoji
    label: String, // what is shown
    count: u32,
    me: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct Msg {
    id: String,
    author: String,
    author_id: String,
    ts: String,
    text: String,
    raw: String, // message content as sent (for editing)
    images: Vec<ImgRef>,
    reply: Option<String>,             // "author: snippet" of the replied-to message
    thread: Option<(String, String)>,  // (thread id, name) started from this message
    reactions: Vec<Reaction>,
}

#[derive(Clone, Debug)]
struct Chan {
    id: String,
    name: String,
    parent: Option<String>, // Some(parent channel id) for threads
}

#[derive(Clone, Debug)]
struct Guild {
    id: String,
    name: String,
    chans: Vec<Chan>,
    open: bool,
}

enum Ev {
    Ready(String, String),
    Guild(Guild),
    Thread(String, Chan),
    ThreadGone(String, String),
    Reaction { ch: String, id: String, key: String, label: String, user: String, add: bool },
    ReactionClear { ch: String, id: String, key: Option<String> },
    Message(String, Msg),
    Updated(String, Msg),
    Image(String, Result<Pic, String>),
    Deleted(String, String),
    History(String, Result<Vec<Msg>, String>),
    Older(String, Result<Vec<Msg>, String>),
    Status(String),
    Fatal(String),
    Login(Result<LoginInfo, String>),
    LoginUrl(String),
}

/// Result of a successful Discord OAuth2 login (identify + guilds).
#[derive(Clone, Debug)]
pub struct LoginInfo {
    pub id: String,
    pub name: String,
    pub guilds: Vec<String>,
}

/// Strip control characters (terminal escape injection) but keep newlines.
fn clean(s: &str) -> String {
    s.chars().filter(|c| *c == '\n' || !c.is_control()).collect()
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn replace_tag(inner: &str, names: &HashMap<&str, &str>) -> Option<String> {
    if let Some(id) = inner.strip_prefix("@&") {
        return is_digits(id).then(|| "@role".to_string());
    }
    if let Some(id) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
        return is_digits(id).then(|| format!("@{}", names.get(id).copied().unwrap_or("user")));
    }
    if let Some(id) = inner.strip_prefix('#') {
        return is_digits(id).then(|| "#channel".to_string());
    }
    let e = inner.strip_prefix("a:").or_else(|| inner.strip_prefix(':'))?;
    let (name, id) = e.rsplit_once(':')?;
    (is_digits(id) && !name.is_empty()).then(|| format!(":{name}:"))
}

/// Turn <@id>, <#id>, <@&id> and <:emoji:id> markup into readable text.
fn expand_markup(s: &str, names: &HashMap<&str, &str>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('>').filter(|&j| j <= 64).and_then(|j| replace_tag(&after[..j], names).map(|r| (j, r))) {
            Some((j, r)) => {
                out.push_str(&r);
                rest = &after[j + 1..];
            }
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Only fetch images from Discord's own media hosts (never leak requests to third parties).
fn is_discord_media(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.contains('@')
        && !host.contains(':')
        && (host.ends_with(".discordapp.net") || host.ends_with(".discordapp.com"))
}

/// First line of a message, shortened, for reply previews and thread names.
fn snippet(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(max).collect();
    if first.chars().count() > max {
        out.push('…');
    }
    out
}

fn emoji_of(e: &Value) -> Option<(String, String)> {
    let name = e.get("name").and_then(Value::as_str).unwrap_or("?");
    Some(match e.get("id").and_then(Value::as_str) {
        Some(id) => (format!("{}:{}", clean(name), clean(id)), format!(":{}:", clean(name))),
        None => (clean(name), clean(name)),
    })
}

fn parse_reactions(v: &Value) -> Vec<Reaction> {
    let Some(a) = v.get("reactions").and_then(Value::as_array) else { return Vec::new() };
    a.iter()
        .filter_map(|r| {
            let (key, label) = emoji_of(r.get("emoji")?)?;
            Some(Reaction {
                key,
                label,
                count: r.get("count").and_then(Value::as_u64).unwrap_or(1) as u32,
                me: r.get("me").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

fn parse_msg(v: &Value) -> Option<Msg> {
    let id = v.get("id")?.as_str()?.to_string();
    let a = v.get("author")?;
    let author_id = a.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let author = a
        .get("global_name")
        .and_then(Value::as_str)
        .or_else(|| a.get("username").and_then(Value::as_str))
        .unwrap_or("?");
    let ts = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|t| t.get(11..16))
        .unwrap_or("--:--")
        .to_string();

    let mut names: HashMap<&str, &str> = HashMap::new();
    if let Some(ms) = v.get("mentions").and_then(Value::as_array) {
        for m in ms {
            if let (Some(id), Some(n)) = (
                m.get("id").and_then(Value::as_str),
                m.get("global_name").and_then(Value::as_str).or_else(|| m.get("username").and_then(Value::as_str)),
            ) {
                names.insert(id, n);
            }
        }
    }
    let raw = clean(v.get("content").and_then(Value::as_str).unwrap_or(""));
    let mut text = expand_markup(&raw, &names);
    let add_line = |text: &mut String, line: String| {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line);
    };

    let mut images = Vec::new();
    if let Some(att) = v.get("attachments").and_then(Value::as_array) {
        for x in att {
            let Some(n) = x.get("filename").and_then(Value::as_str) else { continue };
            let lower = n.to_lowercase();
            let is_img = x.get("content_type").and_then(Value::as_str).is_some_and(|c| c.starts_with("image/"))
                || [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| lower.ends_with(e));
            let url = x.get("proxy_url").or_else(|| x.get("url")).and_then(Value::as_str);
            match (is_img, url) {
                (true, Some(u)) => {
                    let (w, h) = (
                        x.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
                        x.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
                    );
                    add_line(&mut text, format!("[image: {n}]"));
                    images.push(ImgRef { name: clean(n), url: u.to_string(), w, h });
                }
                _ => add_line(&mut text, format!("[file: {n}]")),
            }
        }
    }
    // image embeds (link previews): only Discord's proxied copy is ever used
    if let Some(es) = v.get("embeds").and_then(Value::as_array) {
        for e in es {
            let img = e.get("image").or_else(|| {
                if e.get("type").and_then(Value::as_str) == Some("image") { e.get("thumbnail") } else { None }
            });
            if let Some(i) = img {
                if let Some(u) = i.get("proxy_url").and_then(Value::as_str) {
                    let (w, h) = (
                        i.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
                        i.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
                    );
                    add_line(&mut text, "[image: embed]".to_string());
                    images.push(ImgRef { name: "embed".into(), url: u.to_string(), w, h });
                }
            }
        }
    }
    if v.get("edited_timestamp").is_some_and(|e| !e.is_null()) {
        text.push_str(" (edited)");
    }
    // reply: show "author: snippet" of the referenced message (null when it was deleted)
    let reply = if v.get("message_reference").is_some_and(|r| !r.is_null()) {
        match v.get("referenced_message").and_then(parse_msg) {
            Some(r) => {
                let s = snippet(&r.text, 60);
                Some(format!("{}: {}", r.author, if s.is_empty() { "[attachment]" } else { &s }))
            }
            None if v.get("type").and_then(Value::as_u64) == Some(19) => Some("(original message unavailable)".into()),
            None => None,
        }
    } else {
        None
    };
    let thread = v.get("thread").and_then(|t| {
        Some((clean(t.get("id")?.as_str()?), clean(t.get("name")?.as_str()?)))
    });
    Some(Msg {
        id,
        author: clean(author),
        author_id,
        ts,
        text: clean(&text),
        raw,
        images,
        reply,
        thread,
        reactions: parse_reactions(v),
    })
}

fn is_thread_type(t: Option<u64>) -> bool {
    matches!(t, Some(10..=12))
}

fn parse_thread(t: &Value) -> Option<Chan> {
    if !is_thread_type(t["type"].as_u64()) {
        return None;
    }
    Some(Chan {
        id: t["id"].as_str()?.into(),
        name: clean(t["name"].as_str()?),
        parent: t["parent_id"].as_str().map(String::from),
    })
}

fn chan_label(c: &Chan) -> String {
    if c.parent.is_some() { format!("↳{}", c.name) } else { format!("#{}", c.name) }
}

// ───────────────────────── gateway ─────────────────────────

struct Resume {
    session: String,
    url: String,
    seq: u64,
}

enum Stop {
    Fatal(String),
    Retry(String),
}

fn retry<E: std::fmt::Display>(e: E) -> Stop {
    Stop::Retry(e.to_string())
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn set_timeout(ws: &mut Ws, d: Duration) {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(Some(d));
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.get_mut().set_read_timeout(Some(d));
        }
        _ => {}
    }
}

fn send_hb(ws: &mut Ws, seq: Option<u64>) -> Result<(), Stop> {
    ws.send(Message::Text(json!({"op": 1, "d": seq}).to_string())).map_err(retry)
}

fn gateway_thread(token: String, tx: Sender<Ev>) {
    let mut resume: Option<Resume> = None;
    let mut wait = 1u64;
    loop {
        let t0 = Instant::now();
        match session(&token, &tx, &mut resume) {
            Ok(()) => {}
            Err(Stop::Fatal(m)) => {
                let _ = tx.send(Ev::Fatal(m));
                return;
            }
            Err(Stop::Retry(m)) => {
                let _ = tx.send(Ev::Status(format!("gateway: {m}")));
            }
        }
        if t0.elapsed() > Duration::from_secs(60) {
            wait = 1;
        }
        thread::sleep(Duration::from_secs(wait));
        wait = (wait * 2).min(30);
    }
}

fn session(token: &str, tx: &Sender<Ev>, resume: &mut Option<Resume>) -> Result<(), Stop> {
    let url = match resume {
        Some(r) => format!("{}/?v=10&encoding=json", r.url.trim_end_matches('/')),
        None => GATEWAY.to_string(),
    };
    let (mut ws, _) = tungstenite::connect(url.as_str()).map_err(retry)?;
    set_timeout(&mut ws, Duration::from_millis(250));

    let mut interval: Option<Duration> = None;
    let mut last_hb = Instant::now();
    let mut acked = true;
    let mut seq: Option<u64> = resume.as_ref().map(|r| r.seq);

    loop {
        if let Some(iv) = interval {
            if last_hb.elapsed() >= iv {
                if !acked {
                    return Err(Stop::Retry("heartbeat timeout".into()));
                }
                send_hb(&mut ws, seq)?;
                last_hb = Instant::now();
                acked = false;
            }
        }
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                continue
            }
            Err(e) => return Err(retry(e)),
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(f) => {
                let code = f.map(|f| u16::from(f.code)).unwrap_or(0);
                return Err(match code {
                    4004 => Stop::Fatal("authentication failed (check DISCORD_TOKEN)".into()),
                    4010..=4014 => Stop::Fatal(format!(
                        "gateway refused the connection (code {code}); check intents / token"
                    )),
                    _ => Stop::Retry(format!("closed ({code})")),
                });
            }
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(s) = v["s"].as_u64() {
            seq = Some(s);
            if let Some(r) = resume.as_mut() {
                r.seq = s;
            }
        }
        match v["op"].as_u64() {
            Some(10) => {
                let ms = v["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250);
                interval = Some(Duration::from_millis(ms));
                last_hb = Instant::now();
                acked = true;
                let payload = match resume.as_ref() {
                    Some(r) => json!({"op": 6, "d": {
                        "token": token, "session_id": r.session, "seq": r.seq}}),
                    None => json!({"op": 2, "d": {
                        "token": token,
                        "intents": INTENTS,
                        "properties": {"os": env::consts::OS, "browser": "litecord", "device": "litecord"}
                    }}),
                };
                ws.send(Message::Text(payload.to_string())).map_err(retry)?;
            }
            Some(11) => acked = true,
            Some(1) => {
                send_hb(&mut ws, seq)?;
                last_hb = Instant::now();
                acked = false;
            }
            Some(7) => return Ok(()),
            Some(9) => {
                if !v["d"].as_bool().unwrap_or(false) {
                    *resume = None;
                }
                thread::sleep(Duration::from_secs(2));
                return Ok(());
            }
            Some(0) => dispatch(&v, resume, tx, seq),
            _ => {}
        }
    }
}

/// Put a thread right after its parent channel (and its sibling threads); replace it if it exists.
fn insert_chan(chans: &mut Vec<Chan>, c: Chan) {
    if let Some(old) = chans.iter_mut().find(|x| x.id == c.id) {
        *old = c;
        return;
    }
    let pos = match &c.parent {
        Some(p) => chans
            .iter()
            .rposition(|x| x.id == *p || x.parent.as_deref() == Some(p))
            .map(|i| i + 1)
            .unwrap_or(chans.len()),
        None => chans.len(),
    };
    chans.insert(pos, c);
}

fn dispatch(v: &Value, resume: &mut Option<Resume>, tx: &Sender<Ev>, seq: Option<u64>) {
    let d = &v["d"];
    match v["t"].as_str() {
        Some("READY") => {
            let name = clean(d["user"]["username"].as_str().unwrap_or("bot"));
            if let (Some(s), Some(u)) = (d["session_id"].as_str(), d["resume_gateway_url"].as_str()) {
                *resume = Some(Resume { session: s.into(), url: u.into(), seq: seq.unwrap_or(0) });
            }
            let _ = tx.send(Ev::Ready(name, d["user"]["id"].as_str().unwrap_or("").into()));
        }
        Some("GUILD_CREATE") => {
            let Some(id) = d["id"].as_str() else { return };
            let mut chans: Vec<(i64, Chan)> = d["channels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|c| matches!(c["type"].as_u64(), Some(0) | Some(5)))
                        .filter_map(|c| {
                            Some((
                                c["position"].as_i64().unwrap_or(0),
                                Chan { id: c["id"].as_str()?.into(), name: clean(c["name"].as_str()?), parent: None },
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            chans.sort_by_key(|c| c.0);
            let mut chans: Vec<Chan> = chans.into_iter().map(|c| c.1).collect();
            if let Some(ts) = d["threads"].as_array() {
                for t in ts.iter().filter_map(parse_thread) {
                    insert_chan(&mut chans, t);
                }
            }
            let _ = tx.send(Ev::Guild(Guild {
                id: id.into(),
                name: clean(d["name"].as_str().unwrap_or("?")),
                chans,
                open: false,
            }));
        }
        Some("MESSAGE_CREATE") => {
            if let (Some(ch), Some(m)) = (d["channel_id"].as_str(), parse_msg(d)) {
                let _ = tx.send(Ev::Message(ch.into(), m));
            }
        }
        Some("MESSAGE_UPDATE") => {
            // partial updates (e.g. link unfurls) may lack author/content: parse_msg skips those
            if let (Some(ch), Some(m)) = (d["channel_id"].as_str(), parse_msg(d)) {
                let _ = tx.send(Ev::Updated(ch.into(), m));
            }
        }
        Some("THREAD_CREATE") => {
            if let (Some(g), Some(t)) = (d["guild_id"].as_str(), parse_thread(d)) {
                let _ = tx.send(Ev::Thread(g.into(), t));
            }
        }
        Some("THREAD_UPDATE") => {
            if let (Some(g), Some(t)) = (d["guild_id"].as_str(), parse_thread(d)) {
                // archived threads leave the list (they come back when unarchived)
                let ev = if d["thread_metadata"]["archived"].as_bool().unwrap_or(false) {
                    Ev::ThreadGone(g.into(), t.id)
                } else {
                    Ev::Thread(g.into(), t)
                };
                let _ = tx.send(ev);
            }
        }
        Some("THREAD_DELETE") => {
            if let (Some(g), Some(id)) = (d["guild_id"].as_str(), d["id"].as_str()) {
                let _ = tx.send(Ev::ThreadGone(g.into(), id.into()));
            }
        }
        Some("THREAD_LIST_SYNC") => {
            if let (Some(g), Some(ts)) = (d["guild_id"].as_str(), d["threads"].as_array()) {
                for t in ts.iter().filter_map(parse_thread) {
                    let _ = tx.send(Ev::Thread(g.into(), t));
                }
            }
        }
        Some(t @ ("MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE")) => {
            if let (Some(ch), Some(id), Some(user), Some((key, label))) = (
                d["channel_id"].as_str(),
                d["message_id"].as_str(),
                d["user_id"].as_str(),
                emoji_of(&d["emoji"]),
            ) {
                let _ = tx.send(Ev::Reaction {
                    ch: ch.into(),
                    id: id.into(),
                    key,
                    label,
                    user: user.into(),
                    add: t == "MESSAGE_REACTION_ADD",
                });
            }
        }
        Some("MESSAGE_REACTION_REMOVE_ALL") | Some("MESSAGE_REACTION_REMOVE_EMOJI") => {
            if let (Some(ch), Some(id)) = (d["channel_id"].as_str(), d["message_id"].as_str()) {
                let key = if v["t"].as_str() == Some("MESSAGE_REACTION_REMOVE_EMOJI") {
                    emoji_of(&d["emoji"]).map(|e| e.0)
                } else {
                    None
                };
                let _ = tx.send(Ev::ReactionClear { ch: ch.into(), id: id.into(), key });
            }
        }
        Some("MESSAGE_DELETE") => {
            if let (Some(ch), Some(id)) = (d["channel_id"].as_str(), d["id"].as_str()) {
                let _ = tx.send(Ev::Deleted(ch.into(), id.into()));
            }
        }
        _ => {}
    }
}

// ───────────────────────── REST workers ─────────────────────────
// Two long-lived threads (REST, images) instead of a thread per request:
// fewer allocator arenas, serialized calls (friendlier to rate limits).

enum Job {
    History(String),
    Post { ch: String, text: String, reply: Option<String> },
    Edit { ch: String, id: String, text: String },
    NewThread { ch: String, id: String, name: String },
    Join(String),
    Older { ch: String, before: String },
    React { ch: String, id: String, key: String, add: bool },
    Delete { ch: String, id: String },
}

struct ImgJob {
    key: String,
    url: String,
    w: u32,
    h: u32,
}

fn http_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(403, _) => "403 forbidden (the bot lacks permission here)".into(),
        ureq::Error::Status(404, _) => "404 not found".into(),
        ureq::Error::Status(429, _) => "429 rate limited".into(),
        ureq::Error::Status(c, _) => format!("HTTP {c}"),
        other => other.to_string(),
    }
}

/// Run a request; on 429 wait for Retry-After (max 10 s) and retry once.
fn call(req: impl Fn() -> Result<ureq::Response, ureq::Error>) -> Result<ureq::Response, String> {
    for attempt in 0..2 {
        match req() {
            Ok(r) => return Ok(r),
            Err(ureq::Error::Status(429, r)) if attempt == 0 => {
                let secs = r.header("retry-after").and_then(|s| s.parse::<f64>().ok()).unwrap_or(1.0);
                thread::sleep(Duration::from_secs_f64(secs.clamp(0.1, 10.0)));
            }
            Err(e) => return Err(http_err(e)),
        }
    }
    Err("429 rate limited".into())
}

fn fetch_history(agent: &ureq::Agent, auth: &str, ch: &str, before: Option<&str>) -> Result<Vec<Msg>, String> {
    let url = match before {
        Some(b) if is_digits(b) => format!("{API}/channels/{ch}/messages?limit=50&before={b}"),
        _ => format!("{API}/channels/{ch}/messages?limit=50"),
    };
    let v: Value = call(|| agent.get(&url).set("Authorization", auth).call())?
        .into_json()
        .map_err(|e| e.to_string())?;
    let mut m: Vec<Msg> = v.as_array().map(|a| a.iter().filter_map(parse_msg).collect()).unwrap_or_default();
    m.reverse();
    Ok(m)
}

fn post_message(agent: &ureq::Agent, auth: &str, ch: &str, text: &str, reply: Option<&str>) -> Result<(), String> {
    let url = format!("{API}/channels/{ch}/messages");
    let mut body = json!({ "content": text });
    if let Some(r) = reply {
        body["message_reference"] = json!({ "message_id": r, "fail_if_not_exists": false });
    }
    call(|| agent.post(&url).set("Authorization", auth).send_json(body.clone()))?;
    Ok(())
}

fn edit_message(agent: &ureq::Agent, auth: &str, ch: &str, id: &str, text: &str) -> Result<(), String> {
    let url = format!("{API}/channels/{ch}/messages/{id}");
    call(|| agent.request("PATCH", &url).set("Authorization", auth).send_json(json!({ "content": text })))?;
    Ok(())
}

fn new_thread(agent: &ureq::Agent, auth: &str, ch: &str, id: &str, name: &str) -> Result<(), String> {
    let url = format!("{API}/channels/{ch}/messages/{id}/threads");
    let body = json!({ "name": name, "auto_archive_duration": 1440 });
    call(|| agent.post(&url).set("Authorization", auth).send_json(body.clone()))?;
    Ok(())
}

fn pct(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

fn react_rest(agent: &ureq::Agent, auth: &str, ch: &str, id: &str, key: &str, add: bool) -> Result<(), String> {
    let url = format!("{API}/channels/{ch}/messages/{id}/reactions/{}/@me", pct(key));
    call(|| agent.request(if add { "PUT" } else { "DELETE" }, &url).set("Authorization", auth).send_string(""))?;
    Ok(())
}

fn delete_message(agent: &ureq::Agent, auth: &str, ch: &str, id: &str) -> Result<(), String> {
    let url = format!("{API}/channels/{ch}/messages/{id}");
    call(|| agent.delete(&url).set("Authorization", auth).call())?;
    Ok(())
}

/// A bot only receives a public thread's messages after joining it.
fn join_thread(agent: &ureq::Agent, auth: &str, id: &str) -> Result<(), String> {
    let url = format!("{API}/channels/{id}/thread-members/@me");
    call(|| agent.put(&url).set("Authorization", auth).send_string(""))?;
    Ok(())
}

fn rest_worker(rx: Receiver<Job>, tx: Sender<Ev>, agent: ureq::Agent, auth: String) {
    while let Ok(job) = rx.recv() {
        match job {
            Job::History(ch) => {
                let r = fetch_history(&agent, &auth, &ch, None);
                let _ = tx.send(Ev::History(ch, r));
            }
            Job::Post { ch, text, reply } => {
                if let Err(e) = post_message(&agent, &auth, &ch, &text, reply.as_deref()) {
                    let _ = tx.send(Ev::Status(format!("send failed: {e}")));
                }
            }
            Job::Edit { ch, id, text } => {
                if let Err(e) = edit_message(&agent, &auth, &ch, &id, &text) {
                    let _ = tx.send(Ev::Status(format!("edit failed: {e}")));
                }
            }
            Job::NewThread { ch, id, name } => {
                let msg = match new_thread(&agent, &auth, &ch, &id, &name) {
                    Ok(()) => "thread created".to_string(),
                    Err(e) => format!("thread failed: {e}"),
                };
                let _ = tx.send(Ev::Status(msg));
            }
            Job::Older { ch, before } => {
                let r = fetch_history(&agent, &auth, &ch, Some(&before));
                let _ = tx.send(Ev::Older(ch, r));
            }
            Job::React { ch, id, key, add } => {
                if let Err(e) = react_rest(&agent, &auth, &ch, &id, &key, add) {
                    let _ = tx.send(Ev::Status(format!("reaction failed: {e}")));
                }
            }
            Job::Delete { ch, id } => {
                if let Err(e) = delete_message(&agent, &auth, &ch, &id) {
                    let _ = tx.send(Ev::Status(format!("delete failed: {e}")));
                }
            }
            Job::Join(id) => {
                if let Err(e) = join_thread(&agent, &auth, &id) {
                    let _ = tx.send(Ev::Status(format!("join thread failed: {e}")));
                }
            }
        }
    }
}

fn fetch_image(agent: &ureq::Agent, j: &ImgJob) -> Result<Pic, String> {
    if !is_discord_media(&j.url) {
        return Err("blocked: not a Discord media URL".into());
    }
    // ask Discord's media proxy for a PNG already shrunk to the target size,
    // so we never download or decode a full-size original
    let sep = if j.url.contains('?') { '&' } else { '?' };
    let url = format!("{}{}format=png&width={}&height={}", j.url, sep, j.w, j.h);
    const CAP: u64 = 12 * 1024 * 1024;
    let resp = call(|| agent.get(&url).call())?; // no Authorization header for the CDN
    let mut bytes = Vec::new();
    resp.into_reader().take(CAP).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 >= CAP {
        return Err("image too large".into());
    }
    let pic = image::decode(&bytes)?;
    drop(bytes);
    Ok(image::downscale(pic, j.w, j.h))
}


fn apply_reaction(m: &mut Msg, key: String, label: String, me: bool, add: bool) {
    match m.reactions.iter().position(|r| r.key == key) {
        Some(i) if add => {
            m.reactions[i].count += 1;
            m.reactions[i].me |= me;
        }
        Some(i) => {
            let r = &mut m.reactions[i];
            r.count = r.count.saturating_sub(1);
            if me {
                r.me = false;
            }
            if r.count == 0 {
                m.reactions.remove(i);
            }
        }
        None if add => m.reactions.push(Reaction { key, label, count: 1, me }),
        None => {}
    }
}

fn image_worker(rx: Arc<Mutex<Receiver<ImgJob>>>, tx: Sender<Ev>, agent: ureq::Agent) {
    loop {
        let job = {
            let g = rx.lock().unwrap();
            g.recv()
        };
        let Ok(j) = job else { break };
        let r = fetch_image(&agent, &j);
        let _ = tx.send(Ev::Image(j.key, r));
    }
}

fn main() -> eframe::Result<()> {
    let demo = env::args().any(|a| a == "--demo");
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("litecord"),
        ..Default::default()
    };
    eframe::run_native("litecord", opts, Box::new(move |cc| Box::new(ui::App::new(cc, demo))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_escapes_but_keeps_newlines() {
        assert_eq!(clean("a\x1b[31mb\nc\u{7}"), "a[31mb\nc");
    }

    #[test]
    fn markup_is_expanded() {
        let mut names = HashMap::new();
        names.insert("123", "Alice");
        assert_eq!(expand_markup("hi <@123> and <@!123>!", &names), "hi @Alice and @Alice!");
        assert_eq!(expand_markup("<@999> <@&5> <#7>", &names), "@user @role #channel");
        assert_eq!(expand_markup("<:smile:42> <a:dance:43>", &names), ":smile: :dance:");
        assert_eq!(expand_markup("1 < 2 and <b>x</b> <@abc>", &names), "1 < 2 and <b>x</b> <@abc>");
    }

    #[test]
    fn only_discord_media_hosts_are_fetched() {
        assert!(is_discord_media("https://media.discordapp.net/attachments/1/2/a.png?ex=1&is=2"));
        assert!(is_discord_media("https://cdn.discordapp.com/attachments/1/2/a.png"));
        assert!(!is_discord_media("http://cdn.discordapp.com/a.png"));
        assert!(!is_discord_media("https://evil.example/a.png"));
        assert!(!is_discord_media("https://cdn.discordapp.com.evil.example/a.png"));
        assert!(!is_discord_media("https://cdn.discordapp.com@evil.example/a.png"));
    }

    #[test]
    fn images_and_edits_are_parsed() {
        let v = json!({
            "id": "1", "timestamp": "2026-11-07T09:05:33+00:00",
            "edited_timestamp": "2026-11-07T09:06:00+00:00",
            "author": {"username": "bob"},
            "content": "look <@7>",
            "mentions": [{"id": "7", "username": "alice"}],
            "attachments": [
                {"filename": "cat.PNG", "proxy_url": "https://media.discordapp.net/a/cat.PNG", "width": 640, "height": 480, "content_type": "image/png"},
                {"filename": "notes.pdf", "url": "https://cdn.discordapp.com/a/notes.pdf"}]
        });
        let m = parse_msg(&v).unwrap();
        assert_eq!(m.images.len(), 1);
        assert_eq!((m.images[0].w, m.images[0].h), (640, 480));
        assert!(m.text.starts_with("look @alice"));
        assert!(m.text.contains("[image: cat.PNG]") && m.text.contains("[file: notes.pdf]"));
        assert!(m.text.ends_with("(edited)"));
    }

    #[test]
    fn threads_are_listed_under_their_parent() {
        let (tx, rx) = mpsc::channel();
        let v = json!({"t": "GUILD_CREATE", "d": {"id": "9", "name": "G",
            "channels": [
                {"id": "a", "name": "a", "type": 0, "position": 1},
                {"id": "v", "name": "voice", "type": 2, "position": 0},
                {"id": "b", "name": "b", "type": 0, "position": 2}],
            "threads": [{"id": "t1", "name": "one", "type": 11, "parent_id": "a"}]}});
        dispatch(&v, &mut None, &tx, None);
        let Ev::Guild(g) = rx.try_recv().unwrap() else { panic!("expected guild") };
        let ids: Vec<_> = g.chans.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "t1", "b"]);
    }
}
