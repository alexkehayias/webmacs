# Build stage
FROM rust:bookworm AS builder

WORKDIR /

## Cache rust dependencies
## https://stackoverflow.com/questions/58473606/cache-rust-dependencies-with-docker-build
RUN mkdir ./src && echo 'fn main() { println!("Dummy!"); }' > ./src/main.rs
COPY ./Cargo.toml .
RUN cargo build --release

## Actually build the app
RUN rm -rf ./src
COPY ./src ./src
COPY ./static ./static
RUN touch -a -m ./src/main.rs
RUN cargo build --release

# Run stage
FROM debian:bookworm-slim AS runner

RUN apt-get update && apt install -y openssl

# Use the compiled binary rather than cargo
COPY --from=builder /target/release/webmacs ./webmacs
COPY ./static ./static

EXPOSE 8080

ENTRYPOINT ["./webmacs"]
