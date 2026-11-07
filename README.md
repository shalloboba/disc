# litecord GUI (egui)

軽量Discordクライアント(公式Bot API)。TUI版をeguiのGUIにしたものです。

## Windows exe の作り方

**A. GitHub Actions(おすすめ・何もインストール不要)**
1. このフォルダをそのままGitHubのリポジトリにpush
2. Actions タブ → `build-windows-exe` の成果物 `litecord-windows-x86_64` から `litecord.exe` をダウンロード

**B. 手元のWindowsで**
```
rustup default stable   # Rust 1.80以上
cargo build --release
```
→ `target\release\litecord.exe`(これ1つだけで動きます)

## 使い方
1. 初回起動でBotトークンを入力 →「保存して接続」(設定は `%APPDATA%\litecord\settings.json` に平文保存)
2. 左のサーバー/チャンネルを選んでチャット。画像はメッセージ内に表示され、クリックで拡大(Escで閉じる)
3. メッセージ横の「…」: 返信 / 編集(自分のBot投稿) / スレッド作成 / リアクション / 削除
4. `litecord.exe --demo` でトークンなしの動作確認

## Discordログイン
- Developer Portal → OAuth2 に Redirect `http://127.0.0.1:53682/callback` を登録
- 設定の「クライアントID」を入力(シークレットは空でも可。PKCE使用)
- 上部の「Discordでログイン」→ ブラウザで承認
- スコープは `identify guilds` のみ。**ログインしたユーザーが参加しているサーバーだけが一覧に出ます**。
  投稿はBot名義で送られるため、ログイン中は本文の先頭に `**名前**: ` が付きます。
- OAuth2ではメッセージの読み書き権限は取れません(Discordの仕様)。チャット自体はBotトークンで行います。

## ボイスチャット
Discordのボイスチャンネルには入れません(2026年3月以降、DAVEのE2EE対応が必須でこの規模のクライアントでは実装不可)。
代わりに **litecord同士で直接つなぐボイスルーム**を搭載しています。
- 「ボイス」→ 片方が「ホストになる」(UDPポート待受)、他の人が「参加する」(`アドレス:ポート`)
- ホストが全員の音声を中継。LAN/VPN(Tailscale等)ならそのまま、インターネット越しはポート開放が必要
- 16kHz モノラル / G.711 μ-law(約64kbps)。マイク/スピーカーはOS既定のデバイス
