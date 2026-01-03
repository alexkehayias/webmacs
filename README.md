# webmacs

Terminal emulator for running Emacs in the browser via WebAssembly communicating to a server over websocket. Adapted from the [ghostty-web](https://github.com/coder/ghostty-web) demo.

![Webmacs screenshot](./webmacs-20251230.jpg "Webmacs screenshot")

## Quickstart

### Using Docker

```
docker build . -t webmacs:latest
docker run --rm -p 8080:8080 webmacs:latest
```

### Using Cargo

```
cargo run
```
