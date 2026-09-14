FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    foot weston xvfb python3 libfontconfig1 libxkbcommon0 \
    libxkbcommon-x11-0 libegl1 dbus-x11 fonts-dejavu-core \
    fonts-noto-color-emoji ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
