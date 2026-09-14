# Prismattyc demo recorder box: generic user `demo` on host `prismattyc`.
# Arch matches the operator's glibc so the host-built bins run as-is.
FROM archlinux:base

RUN pacman -Syu --noconfirm \
        curl \
        ffmpeg \
        file \
        heaptrack \
        inetutils \
        fontconfig \
        gcc-libs \
        git \
        glibc \
        libx11 \
        libxcursor \
        libxi \
        libxinerama \
        libxkbcommon \
        libxkbcommon-x11 \
        libxrandr \
        noto-fonts-cjk \
        noto-fonts-emoji \
        grim \
        openbox \
        python \
        sway \
        weston \
        ttf-jetbrains-mono-nerd \
        which \
        wtype \
        xdotool \
        xorg-server-xvfb \
        xorg-xdpyinfo \
        xorg-xsetroot \
    && pacman -Scc --noconfirm \
    && fc-cache -f

# Generic operator. Prompt becomes demo@prismattyc.
RUN useradd --create-home --shell /bin/bash --uid 1000 demo \
    && mkdir -p /home/demo/Desktop /home/demo/demo /home/demo/.config/prismattyc \
               /home/demo/.config/openbox /home/demo/.config/sway \
               /home/demo/.local/bin /home/demo/.kiro/agents \
    && chown -R demo:demo /home/demo

# Prismattyc bins + the two agent CLIs. Copied in by run.sh (gitignored).
COPY bins/prismattyc bins/prismattyc-host bins/pmux bins/pmuxd bins/pmux-attach bins/pmux-mcp \
     bins/claude bins/kiro-cli bins/kiro-cli-chat bins/kiro-cli-term /usr/local/bin/
RUN chmod 755 /usr/local/bin/prismattyc /usr/local/bin/prismattyc-host /usr/local/bin/pmux \
              /usr/local/bin/pmuxd /usr/local/bin/pmux-attach /usr/local/bin/pmux-mcp \
              /usr/local/bin/claude /usr/local/bin/kiro-cli /usr/local/bin/kiro-cli-chat /usr/local/bin/kiro-cli-term

COPY config.toml /home/demo/.config/prismattyc/config.toml
COPY openbox-rc.xml /home/demo/.config/openbox/rc.xml
COPY sway-headless.conf /home/demo/.config/sway/config
COPY kiro-agent.json /home/demo/.kiro/agents/pmux.json
COPY entrypoint.sh /usr/local/bin/demo-entrypoint
RUN chmod 755 /usr/local/bin/demo-entrypoint && chown -R demo:demo /home/demo

USER demo
WORKDIR /home/demo
ENV HOME=/home/demo \
    DISPLAY=:99 \
    XDG_RUNTIME_DIR=/tmp/runtime-demo \
    WINIT_UNIX_BACKEND=x11 \
    COLORTERM=truecolor \
    LANG=C.UTF-8 \
    PATH=/usr/local/bin:/home/demo/.local/bin:/usr/bin:/bin

ENTRYPOINT ["/usr/local/bin/demo-entrypoint"]
CMD ["bash"]
