# The media server as an image. The build context is the REPOSITORY ROOT (the server depends on
# core/ by path):
#
#   docker build -f mediaserver/Dockerfile -t tinyrtc-mediaserver .
#
# Running it. Media is UDP, and a media server wants the host's network: Docker's port publishing
# forwards UDP through a userland proxy (slow, per packet) and rewrites source addresses.
#
#   docker run -d --name tinyrtc --network host --restart unless-stopped \
#     -e TINYRTC_SECRET=<the bearer token of the private API> \
#     -e TINYRTC_PUBLIC_IP=<the address phones send to> \
#     ghcr.io/<owner>/tinyrtc-mediaserver
#
# With the host's network the defaults apply: UDP 0.0.0.0:3478 for phones (open it in the
# firewall) and the private HTTP API on 127.0.0.1:8080 — reachable from the same machine only,
# which is where the app's backend calls it from. Every option of `tinyrtc-mediaserver --help` is a
# variable too (TINYRTC_UDP, TINYRTC_HTTP, ...), or follows the image name; the command line wins. The events go to stderr (`docker logs`), one line each;
# -e TINYRTC_LOG=warn keeps only the warnings. `docker stop` sends SIGTERM: the server closes every
# room connection with a goodbye and exits 0.
#
# Without the host's network (Docker Desktop, a quick try), publish the ports and make the API
# listen inside the container, bound to the host's loopback outside:
#
#   docker run --rm -p 3478:3478/udp -p 127.0.0.1:8080:8080 -e TINYRTC_SECRET=... \
#     -e TINYRTC_PUBLIC_IP=<address> -e TINYRTC_HTTP=0.0.0.0:8080 ghcr.io/<owner>/tinyrtc-mediaserver
#
# On the host, once (mediaserver/README.md, "Socket buffers"): every stream shares one socket, and
# Linux caps its buffer at net.core.rmem_max / wmem_max — a container does not change that:
#
#   sysctl -w net.core.rmem_max=4194304 net.core.wmem_max=4194304

FROM rust:1-bookworm AS build
WORKDIR /src
# What the server is built from: itself, and core/ with its vendored libopus and patched crate.
# (libopus is C: the rust image carries the compiler it needs.)
COPY core core
COPY mediaserver mediaserver
WORKDIR /src/mediaserver
# --locked: the image is exactly mediaserver/Cargo.lock. The cache mounts only speed a rebuild on the
# same builder up; the binary is copied out of the mounted target directory.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/mediaserver/target \
    cargo build --release --locked \
 && cp target/release/tinyrtc-mediaserver /tinyrtc-mediaserver

# glibc and libgcc, no shell, not root. The server opens no file and needs no certificate store:
# it serves UDP and a plain-HTTP API, and calls out to nobody.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tinyrtc-mediaserver /usr/local/bin/tinyrtc-mediaserver
EXPOSE 3478/udp
EXPOSE 8080/tcp
ENTRYPOINT ["/usr/local/bin/tinyrtc-mediaserver"]
