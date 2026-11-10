# vid2audio

A tiny self-hosted web tool that extracts the audio from a short video.
Drop a video in, pick a format, rename the file if you like, download.

- Formats: MP3, M4A (AAC), Opus, WAV, FLAC
- Max length: 3 minutes (checked server-side with `ffprobe`)
- No persistent storage: uploads live in a private temp dir that is deleted when the request ends
- Single small Rust binary, no JavaScript framework (the page is one embedded HTML file)

## Stack

Rust, [axum](https://github.com/tokio-rs/axum) + tokio, the `tempfile` crate, and `ffmpeg`/`ffprobe` doing the actual work.

## Run

Requires `ffmpeg` and `ffprobe` on `PATH` and a Rust toolchain.

```sh
cargo build --release
BIND=127.0.0.1:8444 ./target/release/vid2audio
```

`BIND` defaults to `127.0.0.1:8444`.

## Deploy

`deploy/vid2audio.service` is a hardened systemd unit (private `/tmp`, read-only filesystem, memory and CPU caps).
Adjust `User=` and the paths, then:

```sh
sudo install -m644 deploy/vid2audio.service /etc/systemd/system/
sudo systemctl enable --now vid2audio
```

The app has **no authentication**. Keep it on loopback and put it behind something private,
for example [Tailscale Serve](https://tailscale.com/kb/1242/tailscale-serve):

```sh
sudo tailscale serve --bg --set-path=/audio http://127.0.0.1:8444
```

Open it as `/audio/` (with the trailing slash): the page uses relative URLs.

## API

`POST /convert`, multipart form with `format` (`mp3|m4a|opus|wav|flac`) and `file`.
Returns the audio file. Errors: `413` too long, `422` unreadable or no audio track, `429` busy.

```sh
curl -F format=mp3 -F file=@clip.mp4 http://127.0.0.1:8444/convert -o clip.mp3
```

## Limits

Upload cap 300 MB, 2 concurrent conversions, 120 s per ffmpeg run.
