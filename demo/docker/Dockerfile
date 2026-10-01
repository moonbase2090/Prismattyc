ARG NATIVE_IMAGE=prismattyc-native-tests:latest
FROM ${NATIVE_IMAGE}

USER root
RUN pacman --disable-sandbox -Syu --noconfirm --needed \
        base-devel \
        cargo \
        cmake \
        nodejs \
        npm \
        pkgconf \
        rust \
    && npm install --global @openai/codex \
    && useradd --create-home --shell /bin/bash --uid 1001 demo \
    && mkdir -p /home/demo/.config/prismattyc /home/demo/.config/openbox \
               /home/demo/.local/bin /home/demo/.cache /home/demo/work /home/demo/Desktop \
    && chown -R demo:demo /home/demo

COPY --chown=demo:demo demo/config.toml /home/demo/.config/prismattyc/config.toml
COPY --chown=demo:demo tests/native/docker/openbox-rc.xml /home/demo/.config/openbox/rc.xml
COPY demo/docker/entrypoint.sh /usr/local/bin/prismattyc-demo-entrypoint
RUN chmod 755 /usr/local/bin/prismattyc-demo-entrypoint

USER demo
ENV HOME=/home/demo \
    DISPLAY=:99 \
    XDG_RUNTIME_DIR=/tmp/runtime-demo \
    WINIT_UNIX_BACKEND=x11 \
    COLORTERM=truecolor \
    LANG=C.UTF-8 \
    PATH=/usr/local/bin:/home/demo/.local/bin:/usr/bin:/bin
WORKDIR /home/demo/work
RUN curl -fsSL https://dev.meta.ai/install.sh | sh

ENTRYPOINT ["/usr/local/bin/prismattyc-demo-entrypoint"]
CMD ["bash"]
