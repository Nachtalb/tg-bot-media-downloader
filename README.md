# tg-bot-media-downloader

Telegram bot that saves every photo, video, GIF, video note, sticker and file
you send or forward to it into a local folder.

## What it does

- Downloads into `<destination>/<YYYY-MM-DD>/` (turn off with `--date-subfolders false`).
- File names are `<name>_<file_id>.<ext>`; forwarded media gets `_fwd<origin id>`
  in the name. Names are kept under the 255-byte filesystem limit, and existing
  files are skipped.
- Fixes lying extensions by sniffing the bytes: a `.jpg` that is really an MP4
  gets renamed, and MOV/MKV/AVI/MPEG-TS are remuxed (no re-encode) to MP4 or WebM.
- Crops screenshots down to their content with
  [autocrop](https://github.com/Nachtalb/autocrop-rs): status bars, app chrome
  and letterbox bars are cut from images (JPEG, PNG, WebP) and MP4/WebM videos.
  The cropped file replaces the download. Turn off with `--autocrop false`.
- One live status message per chat shows the queue, errors and totals, with
  buttons to clear errors and resend confirmations.
- Size limits: 20 MB on the public Bot API. With `--local-mode` files up to
  150 MB download directly, up to 2000 MB after a confirm button.

Commands: `/start` shows the queue, `/uptime` shows how long the bot has run.

## Build & run

Needs `ffmpeg` on `PATH` (remux, video crop).

```bash
cargo build --release
./target/release/downloader --token <BOT_TOKEN> -d /path/to/downloads
```

With a local [Bot API server](https://github.com/tdlib/telegram-bot-api):

```bash
./target/release/downloader --token <BOT_TOKEN> --local-mode \
  --telegram-api-server http://localhost:8081 \
  --telegram-webhook-url http://localhost:8443 \
  -d /path/to/downloads --max-concurrent-downloads 100
```

In local mode files are moved straight out of the Bot API server's directory
instead of being downloaded over HTTP.

`downloader --help` lists all options; `--generate-completions <shell>` prints
shell completions. Per-chat stats and pending confirmations are kept in
`stats_<chat>.json` / `queue_<chat>.json` in the working directory.

## Tests

```bash
cargo test
```

The crop test runs ffmpeg on `tests/screenshot.jpg`.
