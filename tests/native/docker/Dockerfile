# Native-window test dependencies. The runner copies the binaries under test.
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

RUN useradd --create-home --shell /bin/bash --uid 1000 tester \
    && mkdir -p /home/tester/.config/prismattyc /home/tester/.config/openbox \
    && chown -R tester:tester /home/tester
COPY config.toml /home/tester/.config/prismattyc/config.toml
COPY openbox-rc.xml /home/tester/.config/openbox/rc.xml
COPY entrypoint.sh /usr/local/bin/native-test-entrypoint
RUN chmod 755 /usr/local/bin/native-test-entrypoint && chown -R tester:tester /home/tester
USER tester
WORKDIR /home/tester
ENV HOME=/home/tester DISPLAY=:99 XDG_RUNTIME_DIR=/tmp/runtime-tester \
    WINIT_UNIX_BACKEND=x11 COLORTERM=truecolor LANG=C.UTF-8 \
    PATH=/usr/local/bin:/home/tester/.local/bin:/usr/bin:/bin
ENTRYPOINT ["/usr/local/bin/native-test-entrypoint"]
CMD ["bash"]
