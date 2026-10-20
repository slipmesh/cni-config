# Packaging only - expects the binary already built for the image's arch and staged at
# target/<TARGETARCH>/release/cni-config, as CI does after
#   cargo build --release --target <arch>-unknown-linux-musl
# on a runner of that architecture. Not a `cargo build` inside this Dockerfile: a full rustc
# build segfaults under QEMU emulation for a non-native --platform.
#
# kube's rustls-based client reads CA certs straight from the filesystem (no compiled-in fallback
# bundle) to verify the in-cluster API server's TLS cert, so a bare `FROM scratch` with nothing at
# /etc/ssl/certs/ca-certificates.crt makes every request fail with "No CA certificates were loaded
# from the system" - pull just that one file from Alpine's `ca-certificates` package, not the rest
# of the distro.
FROM alpine:3.24 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ARG TARGETARCH
COPY target/${TARGETARCH}/release/cni-config /cni-config
ENTRYPOINT ["/cni-config"]
