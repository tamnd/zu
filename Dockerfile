# The container of dx/12 section 6:
#
#   docker run --rm -v "$PWD:/data" ghcr.io/tamnd/zu stat graph.zu1
#
# One statically linked binary in an image with a shell in it. The build
# happens on Alpine so that the link is musl's and the result needs no
# loader at all, which is what makes the same image work when somebody
# copies the binary out of it into a distroless stage of their own. It
# is also why the musl rows of platforms.toml exist: a container is
# where a glibc build turns into a segfault nobody can reproduce.
#
# The compiler is pinned here the way it is pinned everywhere else, out
# of toolchains.toml, and `cargo xtask pins` holds this line to that
# table. An image tagged `latest` is a build whose compiler changes on
# somebody else's schedule, which is a broken release on a day nothing
# was released.
ARG rust=1.97.1
ARG alpine=3.22

FROM rust:${rust}-alpine AS build

# musl-dev is the C library the link needs, and build-base is the
# compiler the vendored C in zstd, sqlite and snappy is built with. They
# are named rather than implied because an Alpine rust image carries
# cargo and not a C toolchain, and the failure without them is a linker
# error two minutes into a five minute build.
RUN apk add --no-cache build-base musl-dev

WORKDIR /src
COPY . .

# `--locked` because an image that resolved its own dependency versions
# is an image that is not the build this commit describes. The whole
# tree is copied in one layer rather than the manifests first: the
# usual dependency-caching dance needs a stub per crate and this
# workspace has seventeen of them, so it buys a warm cache at the price
# of a build that silently succeeds against stubs when a crate is added.
RUN cargo build --release --locked -p zu-cli

FROM alpine:${alpine}

# Certificates because the object store reads over TLS, and nothing
# else: the binary is static and asks the image for no library.
RUN apk add --no-cache ca-certificates

COPY --from=build /src/target/release/zu /usr/local/bin/zu

# A user with no privileges, and a working directory that is where a
# volume is meant to land, so `-v "$PWD:/data"` puts a database where
# the command already is. Running as root inside a container is the
# default nobody chose and the one that writes root-owned files into a
# bind mounted directory on the host.
RUN adduser -D -u 10001 zu && mkdir -p /data && chown zu:zu /data
USER zu
WORKDIR /data

LABEL org.opencontainers.image.title="zu" \
      org.opencontainers.image.description="Embedded property graph database with a GQL engine" \
      org.opencontainers.image.source="https://github.com/tamnd/zu" \
      org.opencontainers.image.licenses="Apache-2.0"

# The binary and not a shell, so `docker run zu stat graph.zu1` reads
# the way the command does. `docker run --entrypoint sh` is still there
# for the times a person wants to look around.
ENTRYPOINT ["zu"]
CMD ["--help"]
