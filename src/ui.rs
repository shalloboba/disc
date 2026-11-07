//! egui front end.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use eframe::egui::{self, Align2, Color32, FontId, Key, RichText, ScrollArea, Sense, TextEdit, Vec2};

use super::*;

const MAX_TEX: usize = 40; // textures kept in memory
const IMG_W: u32 = 640; // size requested from Discord's media proxy
const IMG_H: u32 = 480;
const SHOW_W: f32 = 360.0; // size shown inside the chat
const SHOW_H: f32 = 300.0;

// ───────────────────────── settings ─────────────────────────

#[derive(Default, Clone)]
pub struct Settings {
    pub token: String,
    pub client_id: String,
    pub client_secret: String,
}

impl Settings {
    fn path() -> PathBuf {
        let base = env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("litecord").join("settings.json")
    }

    fn load() -> Settings {
        let mut s = Settings::default();
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                let get = |k: &str| v[k].as_str().unwrap_or("").to_string();
                s.token = get("token");
                s.client_id = get("client_id");
                s.client_secret = get("client_secret");
            }
        }
        if s.token.is_empty() {
            s.token = env::var("DISCORD_TOKEN").unwrap_or_default();
        }
        s
    }

    fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let v = json!({"token": self.token, "client_id": self.client_id, "client_secret": self.client_secret});
        std::fs::write(&p, v.to_string()).map_err(|e| e.to_string())
    }
}

// ───────────────────────── state ─────────────────────────

enum ImgState {
    Loading,
    Ready { tex: egui::TextureHandle, w: u32, h: u32 },
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
enum Pending {
    None,
    Reply { id: String, who: String },
    Edit { id: String },
}

enum Act {
    Reply(String, String),
    Edit(String, String),
    Delete(String),
    React(String, String, bool),
    NewThread(String, String),
    OpenChan(String),
    Zoom(String),
    Older,
}

pub struct App {
    settings: Settings,
    connected: bool,
    demo: bool,
    setup_err: String,

    ev_tx: Sender<Ev>,
    rx: Receiver<Ev>,
    jobs: Option<Sender<Job>>,
    img_jobs: Option<Sender<ImgJob>>,
    ctx: egui::Context,

    guilds: Vec<Guild>,
    cur: Option<(String, String)>,
    msgs: VecDeque<Msg>,
    unread: HashMap<String, u32>,
    loading: bool,
    older_loading: bool,
    cap: usize,
    input: String,
    pending: Pending,
    focus_input: bool,
    confirm_del: Option<String>,
    status: String,
    me: String,

    imgs: HashMap<String, ImgState>,
    img_order: VecDeque<String>,
    zoom: Option<String>,

    login: Option<LoginInfo>,
    login_busy: bool,
    login_url: Option<String>,
    login_cancel: Arc<AtomicBool>,

    voice: Option<voice::Voice>,
    show_voice: bool,
    v_host: bool,
    v_addr: String,
    v_port: String,
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:\\Windows\\Fonts\\meiryo.ttc",
        "C:\\Windows\\Fonts\\YuGothR.ttc",
        "C:\\Windows\\Fonts\\yugothic.ttf",
        "C:\\Windows\\Fonts\\msgothic.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    ];
    for p in candidates {
        if let Ok(bytes) = std::fs::read(p) {
            fonts.font_data.insert("jp".to_owned(), egui::FontData::from_owned(bytes));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(list) = fonts.families.get_mut(&fam) {
                    list.push("jp".to_owned());
                }
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn is_img_tag(l: &str) -> bool {
    l.starts_with("[image: ") && l.ends_with(']')
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, demo: bool) -> App {
        setup_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        // backend -> pump -> UI. The pump wakes egui whenever something arrives.
        let (bt, brx) = mpsc::channel::<Ev>();
        let (tx, rx) = mpsc::channel::<Ev>();
        let c = cc.egui_ctx.clone();
        thread::spawn(move || {
            while let Ok(ev) = brx.recv() {
                if tx.send(ev).is_err() {
                    break;
                }
                c.request_repaint();
            }
        });

        let settings = Settings::load();
        let mut app = App {
            settings,
            connected: false,
            demo: false,
            setup_err: String::new(),
            ev_tx: bt,
            rx,
            jobs: None,
            img_jobs: None,
            ctx: cc.egui_ctx.clone(),
            guilds: Vec::new(),
            cur: None,
            msgs: VecDeque::new(),
            unread: HashMap::new(),
            loading: false,
            older_loading: false,
            cap: MAX_MSGS,
            input: String::new(),
            pending: Pending::None,
            focus_input: false,
            confirm_del: None,
            status: String::new(),
            me: String::new(),
            imgs: HashMap::new(),
            img_order: VecDeque::new(),
            zoom: None,
            login: None,
            login_busy: false,
            login_url: None,
            login_cancel: Arc::new(AtomicBool::new(false)),
            voice: None,
            show_voice: false,
            v_host: false,
            v_addr: String::new(),
            v_port: "50505".into(),
        };
        if demo {
            app.start_demo();
        } else if !app.settings.token.trim().is_empty() {
            app.start_backend();
        }
        app
    }

    // ───────── backend control ─────────

    fn start_backend(&mut self) {
        let token = self.settings.token.trim().trim_start_matches("Bot ").trim().to_string();
        if token.is_empty() {
            self.setup_err = "Botトークンを入力してください".into();
            return;
        }
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).user_agent(UA).build();
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (img_tx, img_rx) = mpsc::channel::<ImgJob>();
        {
            let (t, x) = (token.clone(), self.ev_tx.clone());
            thread::spawn(move || gateway_thread(t, x));
        }
        {
            let (x, a, auth) = (self.ev_tx.clone(), agent.clone(), format!("Bot {token}"));
            thread::spawn(move || rest_worker(job_rx, x, a, auth));
        }
        let shared_rx = Arc::new(Mutex::new(img_rx));
        for _ in 0..3 {
            let (r, x, a) = (shared_rx.clone(), self.ev_tx.clone(), agent.clone());
            thread::spawn(move || image_worker(r, x, a));
        }
        self.jobs = Some(job_tx);
        self.img_jobs = Some(img_tx);
        self.connected = true;
        self.demo = false;
        self.setup_err.clear();
        self.status = "接続中…".into();
    }

    fn start_demo(&mut self) {
        self.demo = true;
        self.connected = true;
        self.me = "me".into();
        self.status = "デモモード: 何も送信されません".into();
        for g in demo_guilds() {
            self.on_event(Ev::Guild(g));
        }
    }

    fn job(&self, j: Job) {
        if let Some(tx) = &self.jobs {
            let _ = tx.send(j);
        }
    }

    // ───────── events ─────────

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Ready(name, id) => {
                self.status = format!("{name} として接続しました");
                self.me = id;
            }
            Ev::Thread(gid, c) => {
                if let Some(g) = self.guilds.iter_mut().find(|g| g.id == gid) {
                    insert_chan(&mut g.chans, c);
                }
            }
            Ev::ThreadGone(gid, id) => {
                if let Some(g) = self.guilds.iter_mut().find(|g| g.id == gid) {
                    g.chans.retain(|c| c.id != id);
                }
                self.unread.remove(&id);
                if self.cur.as_ref().is_some_and(|c| c.0 == id) {
                    self.cur = None;
                    self.msgs.clear();
                    self.pending = Pending::None;
                    self.status = "そのスレッドはアーカイブまたは削除されました".into();
                }
            }
            Ev::Reaction { ch, id, key, label, user, add } => {
                if self.cur.as_ref().is_some_and(|c| c.0 == ch) {
                    let me = user == self.me;
                    if let Some(m) = self.msgs.iter_mut().find(|m| m.id == id) {
                        apply_reaction(m, key, label, me, add);
                    }
                }
            }
            Ev::ReactionClear { ch, id, key } => {
                if self.cur.as_ref().is_some_and(|c| c.0 == ch) {
                    if let Some(m) = self.msgs.iter_mut().find(|m| m.id == id) {
                        match key {
                            Some(k) => m.reactions.retain(|r| r.key != k),
                            None => m.reactions.clear(),
                        }
                    }
                }
            }
            Ev::Guild(mut g) => {
                if let Some(old) = self.guilds.iter_mut().find(|x| x.id == g.id) {
                    g.open = old.open;
                    *old = g;
                } else {
                    self.guilds.push(g);
                    self.guilds.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                }
            }
            Ev::Message(ch, m) => {
                if self.cur.as_ref().is_some_and(|c| c.0 == ch) {
                    self.push_msg(m);
                } else {
                    *self.unread.entry(ch).or_insert(0) += 1;
                }
            }
            Ev::Updated(ch, m) => {
                if self.cur.as_ref().is_some_and(|c| c.0 == ch) {
                    if let Some(old) = self.msgs.iter_mut().find(|x| x.id == m.id) {
                        let mut m = m;
                        if m.reactions.is_empty() {
                            m.reactions = std::mem::take(&mut old.reactions);
                        }
                        if m.thread.is_none() {
                            m.thread = old.thread.take();
                        }
                        if m.reply.is_none() {
                            m.reply = old.reply.take();
                        }
                        *old = m;
                    }
                }
            }
            Ev::Image(key, res) => match res {
                Ok(pic) => {
                    let ci = egui::ColorImage::from_rgb([pic.w as usize, pic.h as usize], &pic.rgb);
                    let tex = self.ctx.load_texture(key.clone(), ci, egui::TextureOptions::LINEAR);
                    self.insert_img(key, ImgState::Ready { tex, w: pic.w, h: pic.h });
                }
                Err(e) => self.insert_img(key, ImgState::Failed(e)),
            },
            Ev::Deleted(ch, id) => {
                if self.cur.as_ref().is_some_and(|c| c.0 == ch) {
                    self.msgs.retain(|m| m.id != id);
                }
            }
            Ev::History(ch, res) => {
                if self.cur.as_ref().is_some_and(|c| c.0 == ch) {
                    self.loading = false;
                    match res {
                        Ok(mut h) => {
                            for x in self.msgs.drain(..) {
                                if !h.iter().any(|y| y.id == x.id) {
                                    h.push(x);
                                }
                            }
                            self.msgs = h.into();
                            while self.msgs.len() > self.cap {
                                self.msgs.pop_front();
                            }
                        }
                        Err(e) => self.status = format!("読み込み失敗: {e}"),
                    }
                }
            }
            Ev::Older(ch, res) => {
                if self.cur.as_ref().is_some_and(|c| c.0 == ch) {
                    self.older_loading = false;
                    match res {
                        Ok(h) => {
                            let mut h: Vec<Msg> =
                                h.into_iter().filter(|y| !self.msgs.iter().any(|x| x.id == y.id)).collect();
                            if h.is_empty() {
                                self.status = "これより古いメッセージはありません".into();
                            } else {
                                h.extend(self.msgs.drain(..));
                                self.msgs = h.into();
                                self.cap = MAX_OLDER;
                                while self.msgs.len() > MAX_OLDER {
                                    self.msgs.pop_front();
                                }
                                self.status = format!("{}件のメッセージを読み込みました", self.msgs.len());
                            }
                        }
                        Err(e) => self.status = format!("読み込み失敗: {e}"),
                    }
                }
            }
            Ev::Status(s) => self.status = s,
            Ev::Fatal(s) => {
                // bad token / refused intents: go back to the setup screen
                self.connected = false;
                self.jobs = None;
                self.img_jobs = None;
                self.setup_err = s;
            }
            Ev::Login(res) => {
                self.login_busy = false;
                self.login_url = None;
                match res {
                    Ok(info) => {
                        self.status = format!("{} としてDiscordにログインしました", info.name);
                        self.login = Some(info);
                    }
                    Err(e) => self.status = format!("ログイン失敗: {e}"),
                }
            }
            Ev::LoginUrl(u) => self.login_url = Some(u),
        }
    }

    fn push_msg(&mut self, m: Msg) {
        self.msgs.push_back(m);
        while self.msgs.len() > self.cap {
            self.msgs.pop_front();
        }
    }

    fn insert_img(&mut self, key: String, st: ImgState) {
        if self.imgs.insert(key.clone(), st).is_none() || !self.img_order.contains(&key) {
            self.img_order.push_back(key);
        }
        while self.img_order.len() > MAX_TEX {
            if let Some(old) = self.img_order.pop_front() {
                self.imgs.remove(&old);
            }
        }
    }

    fn request_image(&mut self, r: &ImgRef) {
        if self.imgs.contains_key(&r.url) {
            return;
        }
        let (w, h) = if r.w == 0 || r.h == 0 { (IMG_W, IMG_H) } else { image::fit(r.w, r.h, IMG_W, IMG_H, false) };
        if r.url.starts_with("demo://") {
            let pic = image::gradient(w.min(320), h.min(200));
            let ci = egui::ColorImage::from_rgb([pic.w as usize, pic.h as usize], &pic.rgb);
            let tex = self.ctx.load_texture(r.url.clone(), ci, egui::TextureOptions::LINEAR);
            self.insert_img(r.url.clone(), ImgState::Ready { tex, w: pic.w, h: pic.h });
            return;
        }
        self.insert_img(r.url.clone(), ImgState::Loading);
        if let Some(tx) = &self.img_jobs {
            let _ = tx.send(ImgJob { key: r.url.clone(), url: r.url.clone(), w, h });
        }
    }

    // ───────── actions ─────────

    fn find_chan(&self, id: &str) -> Option<(usize, usize)> {
        self.guilds
            .iter()
            .enumerate()
            .find_map(|(g, gd)| gd.chans.iter().position(|c| c.id == id).map(|c| (g, c)))
    }

    fn cur_is_thread(&self) -> bool {
        let Some((id, _)) = &self.cur else { return false };
        self.find_chan(id).is_some_and(|(g, c)| self.guilds[g].chans[c].parent.is_some())
    }

    fn open_channel(&mut self, g: usize, c: usize) {
        let ch = &self.guilds[g].chans[c];
        let id = ch.id.clone();
        let is_thread = ch.parent.is_some();
        let title = format!("{} {}", self.guilds[g].name, chan_label(ch));
        let name = ch.name.clone();
        self.cur = Some((id.clone(), title));
        self.msgs = VecDeque::new();
        self.pending = Pending::None;
        self.cap = MAX_MSGS;
        self.older_loading = false;
        self.confirm_del = None;
        self.unread.remove(&id);
        self.focus_input = true;
        if !self.demo && is_thread {
            self.job(Job::Join(id.clone()));
        }
        if self.demo {
            self.msgs = demo_msgs(&name).into();
        } else {
            self.loading = true;
            self.job(Job::History(id));
        }
    }

    fn load_older(&mut self) {
        if self.demo || self.older_loading || self.cur.is_none() {
            return;
        }
        if self.msgs.len() >= MAX_OLDER {
            self.status = "履歴の上限 (500件) に達しました".into();
            return;
        }
        let (Some((ch, _)), Some(first)) = (self.cur.clone(), self.msgs.front()) else { return };
        self.older_loading = true;
        let before = first.id.clone();
        self.job(Job::Older { ch, before });
    }

    fn apply(&mut self, a: Act) {
        let Some((ch, _)) = self.cur.clone() else { return };
        match a {
            Act::Reply(id, who) => {
                self.pending = Pending::Reply { id, who };
                self.focus_input = true;
            }
            Act::Edit(id, raw) => {
                self.input = raw;
                self.pending = Pending::Edit { id };
                self.focus_input = true;
            }
            Act::Delete(id) => {
                if self.confirm_del.as_deref() != Some(id.as_str()) {
                    self.status = "もう一度「削除」を押すと確定します".into();
                    self.confirm_del = Some(id);
                } else {
                    self.confirm_del = None;
                    if self.demo {
                        self.msgs.retain(|m| m.id != id);
                    } else {
                        self.job(Job::Delete { ch, id });
                    }
                }
            }
            Act::React(id, key, add) => {
                if self.demo {
                    if let Some(m) = self.msgs.iter_mut().find(|m| m.id == id) {
                        apply_reaction(m, key.clone(), key, true, add);
                    }
                } else {
                    self.job(Job::React { ch, id, key, add });
                }
            }
            Act::NewThread(id, text) => {
                if self.cur_is_thread() {
                    self.status = "スレッドの中ではスレッドを作れません".into();
                } else if self.demo {
                    self.status = "デモモード: スレッドは作成されません".into();
                } else {
                    let name = snippet(&text, 50);
                    let name = if name.is_empty() { "thread".to_string() } else { name };
                    self.job(Job::NewThread { ch, id, name });
                }
            }
            Act::OpenChan(tid) => match self.find_chan(&tid) {
                Some((g, c)) => self.open_channel(g, c),
                None => self.status = "スレッドが一覧にありません (アーカイブ済み?)".into(),
            },
            Act::Zoom(url) => self.zoom = Some(url),
            Act::Older => self.load_older(),
        }
    }

    fn send(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some((cid, _)) = self.cur.clone() else {
            self.status = "先にチャンネルを選択してください".into();
            return;
        };
        let pending = std::mem::replace(&mut self.pending, Pending::None);
        // while logged in with Discord, tag posts so people can tell who wrote them (the bot posts them)
        let tagged = match (&self.login, &pending) {
            (Some(l), Pending::None | Pending::Reply { .. }) => format!("**{}**: {}", l.name, text),
            _ => text.clone(),
        };
        if tagged.chars().count() > 2000 {
            self.status = "メッセージが長すぎます (最大2000文字)".into();
            self.pending = pending;
            return;
        }
        self.input.clear();
        match pending {
            Pending::Edit { id } => {
                if self.msgs.iter().find(|m| m.id == id).is_some_and(|m| m.raw == text) {
                    self.status = "変更がありません".into();
                } else if self.demo {
                    if let Some(m) = self.msgs.iter_mut().find(|m| m.id == id) {
                        m.text = format!("{text} (edited)");
                        m.raw = text;
                    }
                } else {
                    self.job(Job::Edit { ch: cid, id, text });
                }
            }
            Pending::Reply { id, who } => {
                if self.demo {
                    let quoted = self.msgs.iter().find(|m| m.id == id).map(|m| snippet(&m.text, 60)).unwrap_or_default();
                    let mut m = self.local_msg(tagged);
                    m.reply = Some(format!("{who}: {quoted}"));
                    self.push_msg(m);
                } else {
                    self.job(Job::Post { ch: cid, text: tagged, reply: Some(id) });
                }
            }
            Pending::None => {
                if self.demo {
                    let m = self.local_msg(tagged);
                    self.push_msg(m);
                } else {
                    self.job(Job::Post { ch: cid, text: tagged, reply: None });
                }
            }
        }
    }

    fn local_msg(&self, text: String) -> Msg {
        Msg {
            id: format!("local{}", self.msgs.len()),
            author: "you".into(),
            author_id: self.me.clone(),
            ts: "--:--".into(),
            text: text.clone(),
            raw: text,
            ..Default::default()
        }
    }

    fn start_login(&mut self) {
        let cid = self.settings.client_id.trim().to_string();
        if cid.is_empty() {
            self.status = "設定で「クライアントID」を入力してください".into();
            return;
        }
        self.login_busy = true;
        self.login_url = None;
        self.login_cancel = Arc::new(AtomicBool::new(false));
        let (secret, tx, cancel) = (self.settings.client_secret.trim().to_string(), self.ev_tx.clone(), self.login_cancel.clone());
        thread::spawn(move || oauth::login(cid, secret, tx, cancel));
    }

    // ───────── screens ─────────

    fn ui_setup(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(24.0);
            ui.heading("litecord");
            ui.label("軽量なDiscordクライアント (公式Bot API)");
            ui.add_space(12.0);

            ui.label("Botトークン (Discord Developer Portal → Bot → Reset Token)");
            ui.add(TextEdit::singleline(&mut self.settings.token).password(true).desired_width(460.0));
            ui.add_space(8.0);

            ui.collapsing("Discordログイン (任意)", |ui| {
                ui.label("Developer Portal → OAuth2 で取得します。Redirects には下のURLをそのまま登録してください。");
                ui.horizontal(|ui| {
                    ui.label("クライアントID");
                    ui.add(TextEdit::singleline(&mut self.settings.client_id).desired_width(260.0));
                });
                ui.horizontal(|ui| {
                    ui.label("クライアントシークレット (空でも可: PKCE を使います)");
                });
                ui.add(TextEdit::singleline(&mut self.settings.client_secret).password(true).desired_width(460.0));
                let mut uri = oauth::redirect_uri();
                ui.horizontal(|ui| {
                    ui.label("Redirect URI");
                    ui.add(TextEdit::singleline(&mut uri).desired_width(300.0));
                });
            });

            if !self.setup_err.is_empty() {
                ui.add_space(8.0);
                ui.colored_label(Color32::LIGHT_RED, self.setup_err.clone());
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("保存して接続").clicked() {
                    match self.settings.save() {
                        Ok(()) => {}
                        Err(e) => self.status = format!("設定を保存できません: {e}"),
                    }
                    self.start_backend();
                }
                if ui.button("デモを試す").clicked() {
                    self.start_demo();
                }
            });
            ui.add_space(8.0);
            ui.label(RichText::new(format!("設定の保存先 (平文): {}", Settings::path().display())).weak().small());
        });
    }

    fn ui_main(&mut self, ctx: &egui::Context) {
        let (mut do_login, mut do_logout, mut do_cancel) = (false, false, false);

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.strong("litecord");
                ui.separator();
                if let Some(l) = &self.login {
                    ui.label(format!("ログイン中: {}", l.name));
                    if ui.small_button("ログアウト").clicked() {
                        do_logout = true;
                    }
                } else if self.login_busy {
                    ui.spinner();
                    ui.label("ブラウザで認証してください…");
                    if ui.small_button("キャンセル").clicked() {
                        do_cancel = true;
                    }
                } else if ui.button("Discordでログイン").clicked() {
                    do_login = true;
                }
                ui.separator();
                let active = self.voice.as_ref().is_some_and(|v| !v.is_done());
                let label = if active { "ボイス (接続中)" } else { "ボイス" };
                if ui.selectable_label(self.show_voice, label).clicked() {
                    self.show_voice = !self.show_voice;
                }
                ui.separator();
                ui.label(RichText::new(self.status.clone()).weak());
            });
        });
        if do_login {
            self.start_login();
        }
        if do_logout {
            self.login = None;
        }
        if do_cancel {
            self.login_cancel.store(true, Ordering::Relaxed);
        }

        // server / channel list (filtered to the logged-in user's servers when logged in)
        let mut open: Option<(usize, usize)> = None;
        egui::SidePanel::left("side").resizable(true).default_width(230.0).show(ctx, |ui| {
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if self.guilds.is_empty() {
                    ui.label(RichText::new("サーバーを読み込み中…").weak());
                }
                for g in 0..self.guilds.len() {
                    let gd = &self.guilds[g];
                    if let Some(l) = &self.login {
                        if !l.guilds.contains(&gd.id) {
                            continue;
                        }
                    }
                    let unread_total: u32 = gd.chans.iter().map(|c| self.unread.get(&c.id).copied().unwrap_or(0)).sum();
                    let mut title = RichText::new(gd.name.clone()).strong();
                    if unread_total > 0 {
                        title = title.color(Color32::YELLOW);
                    }
                    egui::CollapsingHeader::new(title).id_source(gd.id.clone()).show(ui, |ui| {
                        for (c, ch) in gd.chans.iter().enumerate() {
                            let n = self.unread.get(&ch.id).copied().unwrap_or(0);
                            let indent = if ch.parent.is_some() { "    ↳ " } else { "# " };
                            let mut t = RichText::new(format!("{indent}{}", ch.name));
                            if n > 0 {
                                t = RichText::new(format!("{indent}{} ({n})", ch.name)).color(Color32::YELLOW);
                            }
                            let selected = self.cur.as_ref().is_some_and(|x| x.0 == ch.id);
                            if ui.selectable_label(selected, t).clicked() {
                                open = Some((g, c));
                            }
                        }
                    });
                }
            });
        });
        if let Some((g, c)) = open {
            self.open_channel(g, c);
        }

        // input box
        let (mut send, mut cancel_pending) = (false, false);
        egui::TopBottomPanel::bottom("input").show(ctx, |ui| {
            ui.add_space(4.0);
            match &self.pending {
                Pending::Reply { who, .. } => {
                    ui.horizontal(|ui| {
                        ui.label(format!("{who} に返信中"));
                        if ui.small_button("キャンセル").clicked() {
                            cancel_pending = true;
                        }
                    });
                }
                Pending::Edit { .. } => {
                    ui.horizontal(|ui| {
                        ui.label("メッセージを編集中");
                        if ui.small_button("キャンセル").clicked() {
                            cancel_pending = true;
                        }
                    });
                }
                Pending::None => {}
            }
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 70.0).max(100.0);
                let r = ui.add(
                    TextEdit::singleline(&mut self.input).hint_text("メッセージを入力 (Enterで送信)").desired_width(w),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if ui.button("送信").clicked() || enter {
                    send = true;
                }
                if send || self.focus_input {
                    r.request_focus();
                    self.focus_input = false;
                }
            });
            ui.add_space(4.0);
        });
        if cancel_pending {
            if matches!(self.pending, Pending::Edit { .. }) {
                self.input.clear();
            }
            self.pending = Pending::None;
        }
        if send {
            self.send();
        }

        // messages
        egui::CentralPanel::default().show(ctx, |ui| {
            let Some((_, title)) = self.cur.clone() else {
                ui.centered_and_justified(|ui| {
                    ui.label("左のサーバーからチャンネルを選択してください");
                });
                return;
            };
            let mut head = title;
            if self.loading {
                head.push_str("  (読み込み中…)");
            }
            ui.label(RichText::new(head).strong());
            ui.separator();

            let msgs = std::mem::take(&mut self.msgs);
            let mut acts: Vec<Act> = Vec::new();
            ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                if !self.demo && ui.small_button("古いメッセージを読み込む").clicked() {
                    acts.push(Act::Older);
                }
                for m in msgs.iter() {
                    self.draw_msg(ui, m, &mut acts);
                    ui.add_space(6.0);
                }
            });
            self.msgs = msgs;
            for a in acts {
                self.apply(a);
            }
        });

        self.ui_windows(ctx);
    }

    fn draw_msg(&mut self, ui: &mut egui::Ui, m: &Msg, acts: &mut Vec<Act>) {
        let own = !self.me.is_empty() && m.author_id == self.me;
        ui.horizontal(|ui| {
            ui.label(RichText::new(m.ts.clone()).weak().small());
            let col = if own { Color32::from_rgb(250, 200, 80) } else { Color32::from_rgb(110, 200, 255) };
            ui.label(RichText::new(m.author.clone()).strong().color(col));
            ui.menu_button("…", |ui| {
                if ui.button("返信").clicked() {
                    acts.push(Act::Reply(m.id.clone(), m.author.clone()));
                    ui.close_menu();
                }
                if own && ui.button("編集").clicked() {
                    acts.push(Act::Edit(m.id.clone(), m.raw.clone()));
                    ui.close_menu();
                }
                if m.thread.is_none() && ui.button("スレッドを作成").clicked() {
                    acts.push(Act::NewThread(m.id.clone(), m.text.clone()));
                    ui.close_menu();
                }
                ui.horizontal(|ui| {
                    for e in ["👍", "❤️", "😂", "🎉"] {
                        if ui.button(e).clicked() {
                            acts.push(Act::React(m.id.clone(), e.to_string(), true));
                            ui.close_menu();
                        }
                    }
                });
                if ui.button("削除").clicked() {
                    acts.push(Act::Delete(m.id.clone()));
                    ui.close_menu();
                }
            });
        });

        if let Some(r) = &m.reply {
            ui.label(RichText::new(format!("返信先 {r}")).weak().small());
        }

        let body: String = if m.images.is_empty() {
            m.text.clone()
        } else {
            m.text.lines().filter(|l| !is_img_tag(l)).collect::<Vec<_>>().join("\n")
        };
        if !body.trim().is_empty() {
            ui.label(body);
        }

        // inline images: only requested once they scroll into view
        let mut want: Vec<ImgRef> = Vec::new();
        for r in &m.images {
            let (dw, dh) = if r.w == 0 || r.h == 0 {
                (SHOW_W, 200.0)
            } else {
                let (w, h) = (r.w as f32, r.h as f32);
                let s = (SHOW_W / w).min(SHOW_H / h).min(1.0);
                (w * s, h * s)
            };
            match self.imgs.get(&r.url) {
                Some(ImgState::Ready { tex, w, h }) => {
                    let (w, h) = (*w as f32, *h as f32);
                    let s = (SHOW_W / w).min(SHOW_H / h).min(1.0);
                    let img = egui::Image::new(egui::load::SizedTexture::new(tex.id(), Vec2::new(w * s, h * s)))
                        .sense(Sense::click());
                    let resp = ui.add(img).on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        acts.push(Act::Zoom(r.url.clone()));
                    }
                }
                Some(ImgState::Failed(e)) => {
                    ui.label(RichText::new(format!("画像を読み込めません ({}): {e}", r.name)).color(Color32::LIGHT_RED).small());
                }
                _ => {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(dw, dh), Sense::hover());
                    ui.painter().rect_filled(rect, 4.0, Color32::from_gray(40));
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, "読み込み中…", FontId::proportional(13.0), Color32::GRAY);
                    if ui.is_rect_visible(rect) {
                        want.push(r.clone());
                    }
                }
            }
        }
        for r in want {
            self.request_image(&r);
        }

        if let Some((tid, name)) = &m.thread {
            if ui.link(format!("スレッド: {name}")).clicked() {
                acts.push(Act::OpenChan(tid.clone()));
            }
        }

        if !m.reactions.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for r in &m.reactions {
                    let mut b = egui::Button::new(format!("{} {}", r.label, r.count)).small();
                    if r.me {
                        b = b.fill(Color32::from_rgb(80, 70, 20));
                    }
                    if ui.add(b).clicked() {
                        acts.push(Act::React(m.id.clone(), r.key.clone(), !r.me));
                    }
                }
            });
        }
    }

    fn ui_windows(&mut self, ctx: &egui::Context) {
        // sign-in link fallback
        if self.login_busy {
            if let Some(url) = self.login_url.clone() {
                egui::Window::new("Discordログイン").collapsible(false).resizable(false).show(ctx, |ui| {
                    ui.label("ブラウザが開かない場合は、このURLをブラウザに貼り付けてください。");
                    let mut u = url.clone();
                    ui.add(TextEdit::singleline(&mut u).desired_width(420.0));
                    if ui.button("URLをコピー").clicked() {
                        ui.output_mut(|o| o.copied_text = url.clone());
                    }
                });
            }
        }

        // image viewer
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.zoom = None;
        }
        if let Some(url) = self.zoom.clone() {
            let mut open = true;
            egui::Window::new("画像").open(&mut open).collapsible(false).resizable(true).show(ctx, |ui| {
                match self.imgs.get(&url) {
                    Some(ImgState::Ready { tex, w, h }) => {
                        let avail = ctx.screen_rect().size() * 0.8;
                        let (w, h) = (*w as f32, *h as f32);
                        let s = (avail.x / w).min(avail.y / h).min(2.0);
                        ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), Vec2::new(w * s, h * s))));
                    }
                    Some(ImgState::Failed(e)) => {
                        ui.label(format!("読み込めません: {e}"));
                    }
                    _ => {
                        ui.label("読み込み中…");
                    }
                }
            });
            if !open {
                self.zoom = None;
            }
        }

        // voice
        if self.show_voice {
            let mut open = true;
            egui::Window::new("ボイスチャット").open(&mut open).resizable(false).show(ctx, |ui| self.ui_voice(ui));
            self.show_voice = open;
        }
        if self.voice.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn ui_voice(&mut self, ui: &mut egui::Ui) {
        ui.set_width(380.0);
        ui.label(
            RichText::new(
                "Discordのボイスチャンネルではなく、litecord同士で直接つなぐボイスルームです。\nホストがUDPポートを待ち受け、参加者は「アドレス:ポート」で接続します。\n(インターネット越しはポート開放かVPNが必要。LANならそのまま使えます)",
            )
            .weak()
            .small(),
        );
        ui.add_space(6.0);

        let mut stop = false;
        let mut start: Option<voice::Mode> = None;
        if let Some(v) = &self.voice {
            ui.label(v.status());
            let sh = &v.shared;
            let mut mute = sh.mute.load(Ordering::Relaxed);
            if ui.checkbox(&mut mute, "マイクをミュート").changed() {
                sh.mute.store(mute, Ordering::Relaxed);
            }
            let mut deaf = sh.deafen.load(Ordering::Relaxed);
            if ui.checkbox(&mut deaf, "スピーカーをミュート").changed() {
                sh.deafen.store(deaf, Ordering::Relaxed);
            }
            let lvl = if mute { 0.0 } else { sh.level.load(Ordering::Relaxed) as f32 / 1000.0 };
            ui.add(egui::ProgressBar::new(lvl.sqrt().min(1.0)).desired_width(300.0).text("マイク入力"));
            ui.add_space(4.0);
            if ui.button("停止").clicked() {
                stop = true;
            }
        } else {
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.v_host, true, "ホストになる");
                ui.radio_value(&mut self.v_host, false, "参加する");
            });
            if self.v_host {
                ui.horizontal(|ui| {
                    ui.label("UDPポート");
                    ui.add(TextEdit::singleline(&mut self.v_port).desired_width(80.0));
                });
                if ui.button("ホスト開始").clicked() {
                    match self.v_port.trim().parse::<u16>() {
                        Ok(p) if p > 0 => start = Some(voice::Mode::Host(p)),
                        _ => self.status = "ポート番号が正しくありません".into(),
                    }
                }
            } else {
                ui.horizontal(|ui| {
                    ui.label("接続先");
                    ui.add(TextEdit::singleline(&mut self.v_addr).hint_text("例: 192.168.0.10:50505").desired_width(220.0));
                });
                if ui.button("参加").clicked() {
                    if self.v_addr.trim().is_empty() {
                        self.status = "接続先を入力してください".into();
                    } else {
                        start = Some(voice::Mode::Join(self.v_addr.trim().to_string()));
                    }
                }
            }
        }
        if stop {
            if let Some(v) = self.voice.take() {
                v.stop();
            }
        }
        if let Some(mode) = start {
            self.voice = Some(voice::Voice::start(mode));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(ev) = self.rx.try_recv() {
            self.on_event(ev);
        }
        if self.connected {
            self.ui_main(ctx);
        } else {
            self.ui_setup(ctx);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(v) = self.voice.take() {
            v.stop();
        }
    }
}

// ───────────────────────── demo data ─────────────────────────

fn demo_guilds() -> Vec<Guild> {
    let mk = |id: &str, name: &str, chans: &[&str]| Guild {
        id: id.into(),
        name: name.into(),
        chans: chans.iter().map(|c| Chan { id: format!("{id}-{c}"), name: (*c).into(), parent: None }).collect(),
        open: false,
    };
    let mut g = vec![mk("1", "Rust Japan", &["general", "random", "help"]), mk("2", "Lite Club", &["dev", "chat"])];
    insert_chan(&mut g[0].chans, Chan { id: "1-general-th".into(), name: "wrapping test".into(), parent: Some("1-general".into()) });
    g
}

fn demo_msgs(chan: &str) -> Vec<Msg> {
    let lines = [
        ("alice", "09:01", "おはようございます。今日も軽量クライアントの話をしましょう。"),
        ("bob", "09:03", "Electron は 1GB 近く使うこともあるのに、これは数十MBで動くらしい。"),
        ("alice", "09:04", "A very long line to check that wrapping works properly across the whole width of the window without breaking anything."),
        ("carol", "09:10", "line one\nline two\nline three"),
        ("me", "09:12", "これは自分のメッセージ (…メニューから編集できます)"),
    ];
    let rx = |key: &str, label: &str, count: u32, me: bool| Reaction { key: key.into(), label: label.into(), count, me };
    lines
        .iter()
        .enumerate()
        .map(|(i, (a, t, x))| {
            let text = if i == 3 { format!("{x}\n[image: demo.png]") } else { format!("{x} (#{chan})") };
            Msg {
                id: format!("{chan}{i}"),
                author: (*a).into(),
                author_id: if *a == "me" { "me".into() } else { format!("u-{a}") },
                ts: (*t).into(),
                raw: text.clone(),
                text,
                images: if i == 3 {
                    vec![ImgRef { name: "demo.png".into(), url: "demo://gradient".into(), w: 320, h: 200 }]
                } else {
                    Vec::new()
                },
                reply: if i == 1 { Some("alice: おはようございます。今日も軽量クライアントの話を…".into()) } else { None },
                thread: if i == 2 && chan == "general" { Some(("1-general-th".into(), "wrapping test".into())) } else { None },
                reactions: match i {
                    0 => vec![rx("👍", "👍", 3, false), rx("party:1", ":party:", 1, true)],
                    3 => vec![rx("🎉", "🎉", 12, false)],
                    _ => Vec::new(),
                },
            }
        })
        .collect()
}
