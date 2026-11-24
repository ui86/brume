# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM alpine:3.24 AS release
ARG TARGETARCH
COPY archives/brume-linux-${TARGETARCH}.tar.gz /tmp/brume.tar.gz
RUN mkdir -p /out/bin /out/run \
    && chmod 0700 /out/run \
    && tar -xzf /tmp/brume.tar.gz -C /out/bin brume \
    && chmod 0755 /out/bin/brume

FROM alpine:3.24
COPY --from=release /out/bin/brume /usr/local/bin/brume
COPY --from=release --chown=65532:65532 /out/run/ /run/brume/
COPY --chmod=0755 entrypoint.sh /usr/local/bin/brume-entrypoint
USER 65532:65532
EXPOSE 1080/tcp 1080/udp
ENTRYPOINT ["/usr/local/bin/brume-entrypoint"]
