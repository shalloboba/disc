//! "Login with Discord": OAuth2 authorization-code flow with PKCE and a loopback redirect.
//!
//! Scopes: `identify guilds` only. This proves who the person is and which servers they are in.
//! It does NOT give access to messages (Discord does not allow that for normal OAuth2 apps),
//! so chatting still goes through the Bot token.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{clean, is_digits, pct, Ev, LoginInfo, API, UA};

/// Register exactly this URL under OAuth2 -> Redirects in the Discord developer portal.
pub const PORT: u16 = 53682;

pub fn redirect_uri() -> String {
    format!("http://127.0.0.1:{PORT}/callback")
}

fn rand_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).map_err(|e| e.to_string())?;
    Ok(b)
}

/// Runs on its own thread; always ends by sending `Ev::Login`.
pub fn login(client_id: String, secret: String, tx: Sender<Ev>, cancel: Arc<AtomicBool>) {
    let r = run(&client_id, &secret, &tx, &cancel);
    let _ = tx.send(Ev::Login(r));
}

fn run(client_id: &str, secret: &str, tx: &Sender<Ev>, cancel: &AtomicBool) -> Result<LoginInfo, String> {
    if !is_digits(client_id) {
        return Err("クライアントIDは数字だけです (Developer Portal の Application ID)".into());
    }
    let listener = TcpListener::bind(("127.0.0.1", PORT))
        .map_err(|e| format!("ポート {PORT} を開けません (別のログインが進行中?): {e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;

    let verifier = URL_SAFE_NO_PAD.encode(rand_bytes::<32>()?);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state: String = rand_bytes::<16>()?.iter().map(|b| format!("{b:02x}")).collect();
    let redirect = redirect_uri();
    let url = format!(
        "https://discord.com/oauth2/authorize?client_id={client_id}&response_type=code&redirect_uri={}&scope={}&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        pct(&redirect),
        pct("identify guilds"),
    );
    let _ = tx.send(Ev::LoginUrl(url.clone()));
    open_browser(&url);

    let code = wait_callback(&listener, &state, cancel)?;

    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).user_agent(UA).build();
    let mut form: Vec<(&str, &str)> = vec![
        ("client_id", client_id),
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", &redirect),
        ("code_verifier", &verifier),
    ];
    if !secret.is_empty() {
        form.push(("client_secret", secret));
    }
    let tok: Value = agent
        .post(&format!("{API}/oauth2/token"))
        .send_form(&form)
        .map_err(http_detail)?
        .into_json()
        .map_err(|e| e.to_string())?;
    let access = tok["access_token"].as_str().ok_or("トークン応答に access_token がありません")?.to_string();
    let auth = format!("Bearer {access}");

    let me: Value = agent
        .get(&format!("{API}/users/@me"))
        .set("Authorization", &auth)
        .call()
        .map_err(http_detail)?
        .into_json()
        .map_err(|e| e.to_string())?;
    let guilds: Value = agent
        .get(&format!("{API}/users/@me/guilds"))
        .set("Authorization", &auth)
        .call()
        .map_err(http_detail)?
        .into_json()
        .map_err(|e| e.to_string())?;

    // We only needed the identity once: give the token back (best effort).
    let mut rev: Vec<(&str, &str)> = vec![("client_id", client_id), ("token", &access)];
    if !secret.is_empty() {
        rev.push(("client_secret", secret));
    }
    let _ = agent.post(&format!("{API}/oauth2/token/revoke")).send_form(&rev);

    let id = me["id"].as_str().ok_or("ユーザーIDを取得できません")?.to_string();
    let name = me["global_name"]
        .as_str()
        .or_else(|| me["username"].as_str())
        .map(clean)
        .unwrap_or_else(|| "user".into());
    let guilds = guilds
        .as_array()
        .map(|a| a.iter().filter_map(|g| g["id"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    Ok(LoginInfo { id, name, guilds })
}

fn http_detail(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let body: String = body.chars().take(200).collect();
            format!("HTTP {code}: {body}")
        }
        other => other.to_string(),
    }
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

fn respond(s: &mut TcpStream, code: u16, text: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>litecord</title><body style=\"font-family:sans-serif;margin:3em\"><h2>{text}</h2>"
    );
    let head = format!(
        "HTTP/1.1 {code} OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body.as_bytes());
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|x| u8::from_str_radix(x, 16).ok());
                match h {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn wait_callback(listener: &TcpListener, state: &str, cancel: &AtomicBool) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("キャンセルしました".into());
        }
        if Instant::now() > deadline {
            return Err("タイムアウトしました (3分)".into());
        }
        match listener.accept() {
            Ok((mut s, _)) => {
                let _ = s.set_nonblocking(false);
                let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or("");
                let target = line.split_whitespace().nth(1).unwrap_or("");
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                if path != "/callback" {
                    respond(&mut s, 404, "not found");
                    continue;
                }
                let (mut code, mut st, mut err) = (None, None, None);
                for kv in query.split('&') {
                    if let Some((k, v)) = kv.split_once('=') {
                        let v = pct_decode(v);
                        match k {
                            "code" => code = Some(v),
                            "state" => st = Some(v),
                            "error" => err = Some(v),
                            _ => {}
                        }
                    }
                }
                if st.as_deref() != Some(state) {
                    respond(&mut s, 400, "state が一致しません");
                    continue; // stray request: keep waiting for the real one
                }
                if let Some(e) = err {
                    respond(&mut s, 200, "ログインがキャンセルされました");
                    return Err(format!("Discord: {e}"));
                }
                if let Some(c) = code {
                    respond(&mut s, 200, "ログインしました。このタブは閉じて構いません。");
                    return Ok(c);
                }
                respond(&mut s, 400, "code がありません");
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(pct_decode("a%20b+c"), "a b c");
        assert_eq!(pct_decode("100%"), "100%");
        assert_eq!(pct_decode("%zz"), "%zz");
        assert_eq!(pct_decode("%41"), "A");
    }

    #[test]
    fn redirect_is_loopback() {
        assert_eq!(redirect_uri(), "http://127.0.0.1:53682/callback");
    }
}
