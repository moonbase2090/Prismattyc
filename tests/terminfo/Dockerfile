FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 ncurses-bin openssh-server openssh-client less vim && rm -rf /var/lib/apt/lists/*
COPY . /src
WORKDIR /src
CMD ["sh", "tests/terminfo/entrypoint.sh"]
