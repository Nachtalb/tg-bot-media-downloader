FROM alpine:3.22 AS ffmpeg
ARG FFMPEG_VERSION=7.0.2
ARG FFMPEG_SHA256=abda8d77ce8309141f83ab8edf0596834087c52467f6badf376a6a2a4c87cf67
RUN apk add --no-cache curl tar xz tzdata \
 && curl -fsSL "https://johnvansickle.com/ffmpeg/releases/ffmpeg-${FFMPEG_VERSION}-amd64-static.tar.xz" -o /ff.tar.xz \
 && echo "${FFMPEG_SHA256}  /ff.tar.xz" | sha256sum -c - \
 && tar -xJf /ff.tar.xz -C /usr/local/bin --strip-components=1 --wildcards '*/ffmpeg' \
 && rm /ff.tar.xz \
 && mkdir -p -m 1777 /rootfs/tmp && mkdir -p /rootfs/data && chown 3002:3002 /rootfs/data

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src src
# COPY keeps mtimes older than the stub build above; without the touch cargo ships the stub.
RUN touch src/main.rs && cargo build --release --locked

FROM scratch
COPY --from=ffmpeg /usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=ffmpeg /rootfs/ /
COPY --from=ffmpeg /usr/local/bin/ffmpeg /usr/local/bin/ffmpeg
COPY --from=build /src/target/release/downloader /usr/local/bin/downloader
ENV PATH=/usr/local/bin TZ=Europe/Zurich RUST_LOG=info
# stats_<chat>.json / queue_<chat>.json are written to the working directory.
WORKDIR /data
USER 3002:3002
# The bot shuts down cleanly on Ctrl-C only; as PID 1 it would ignore SIGTERM.
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/downloader"]
