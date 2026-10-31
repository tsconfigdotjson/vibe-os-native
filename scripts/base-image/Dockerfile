# The base image's root filesystem (PRD: What's inside linux): Debian slim
# with git, gh, curl, ripgrep, jq, build-essential, Python 3, Node LTS and
# mise; bubblewrap and socat, for Claude Code's own sandbox; and sudo, which
# vibe-guest lets the user `vibe` use in the toolbox shell only (vibe.sudo),
# for `sudo apt install …` there.
# scripts/base-image.sh builds it for linux/arm64 and exports it into
# a read-only ext4 image. Downloads are pinned by checksum.
FROM debian:trixie-slim

ARG NODE_VERSION=26.13.0
ARG NODE_SHA256=406fe3fbde1c054adff8b297c16fdb5af6659db327c1ac8acd46b56ef1af9a92
ARG MISE_VERSION=2026.10.24
ARG MISE_SHA256=ae1151a87d70146119f68f7378001a12f09d2a38853087ed6139b7527a55f148

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git gh curl ca-certificates ripgrep jq build-essential \
      python3 python3-pip python3-venv \
      openssh-client less procps psmisc unzip xz-utils zstd file \
      bubblewrap socat sudo \
 && rm -rf /var/lib/apt/lists/*

# Node LTS, from nodejs.org.
RUN curl -fsSLo /tmp/node.tar.xz \
      "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-arm64.tar.xz" \
 && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
 && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
      --exclude=CHANGELOG.md --exclude=LICENSE --exclude=README.md \
 && rm /tmp/node.tar.xz \
 && node --version && npm --version

# mise, from its GitHub release.
RUN curl -fsSLo /usr/local/bin/mise \
      "https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-arm64" \
 && echo "${MISE_SHA256}  /usr/local/bin/mise" | sha256sum -c - \
 && chmod 755 /usr/local/bin/mise \
 && mise --version

# Where the managed layer puts Claude Code, first on PATH.
RUN install -d -m 755 /opt/vibeos/bin
