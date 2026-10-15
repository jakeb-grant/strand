# The CI `check` job's machine (.github/workflows/ci.yml) as an image:
# Ubuntu 24.04 (the pixel references come from its sway 1.9,
# fonts-dejavu-core 2.37 and adwaita-icon-theme 46), the job's apt
# packages, the same fonts-dejavu-extra removal and Rust 1.97.0.
# Built and run by scripts/container/run.sh.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
# The CI check job's install step, package for package, plus git, jq,
# ca-certificates and curl (rustup, the matrix test's build parsing).
# The GPU tier (M4): Mesa's lavapipe (software Vulkan) with the loader
# and vulkaninfo; libpam0g-dev for strand-auth's PAM helper.
RUN apt-get update \
 && apt-get install -y --no-install-recommends sway grim dbus fonts-dejavu-core adwaita-icon-theme pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev \
      dbus-daemon python3-dbusmock pipewire pipewire-pulse pipewire-bin wireplumber pulseaudio-utils libpipewire-0.3-dev libspa-0.2-dev clang libclang-dev \
      mesa-vulkan-drivers libvulkan1 vulkan-tools libpam0g-dev \
      git jq ca-certificates curl \
 && (dpkg --remove --force-depends fonts-dejavu fonts-dejavu-extra || true) \
 && rm -rf /var/lib/apt/lists/*

# rustup into /opt, readable by any uid (run.sh runs as the host user).
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain 1.97.0 -c rustfmt -c clippy \
 && rustc --version && cargo fmt --version && cargo clippy --version \
 && chmod -R a+rwX /opt/rustup /opt/cargo

# The cargo registry and git checkouts live in named volumes
# (run.sh); a fresh volume copies these directories' modes, so any uid
# can write to it.
RUN mkdir -p /opt/cargo/registry /opt/cargo/git && chmod 0777 /opt/cargo/registry /opt/cargo/git
ENV LANG=C.UTF-8

# Lavapipe is the only Vulkan driver the loader offers (as in CI's env,
# .github/workflows/ci.yml): the image also ships Mesa's hardware ICDs,
# which must never be picked up (scripts/container/gpu.sh selects a
# hardware ICD on purpose). VK_ICD_FILENAMES is the deprecated spelling
# older loaders read. A fixed llvmpipe vector width keeps GPU-tier pixels
# the same on AVX2 and AVX-512 machines.
ENV VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
    LP_NATIVE_VECTOR_WIDTH=256

# The tree-sitter CLI (M5; docs/architecture.md, "tree-sitter layout"):
# a pinned prebuilt release checked by its sha256, for the lint job's
# check that the committed parser.c and grammar.json match grammar.js
# (scripts/check-tree-sitter.sh). `generate --js-runtime native`
# evaluates grammar.js with the CLI's built-in QuickJS, so no node
# (decisions.md m5-infra, spike 2). The same version and sha256 are in
# .github/workflows/ci.yml's lint job; edit both together
# (scripts/container/test-check-tree-sitter.sh compares them).
ARG TREE_SITTER_VERSION=0.27.0
ARG TREE_SITTER_SHA256=20a1f39ec1c45f2211492dcb8881c802b643b554bb196869a29ac3778277fa77
RUN curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/tree-sitter.gz \
      "https://github.com/tree-sitter/tree-sitter/releases/download/v${TREE_SITTER_VERSION}/tree-sitter-linux-x64.gz" \
 && echo "${TREE_SITTER_SHA256}  /tmp/tree-sitter.gz" | sha256sum -c - \
 && gunzip -c /tmp/tree-sitter.gz >/usr/local/bin/tree-sitter \
 && chmod 0755 /usr/local/bin/tree-sitter \
 && rm /tmp/tree-sitter.gz \
 && tree-sitter --version

# sccache, the laptop's compile cache (decisions.md, ci-speed): run.sh
# wraps rustc with it, the cache in the named volume strand-sccache, so a
# fresh worktree takes its dependencies from the cache instead of
# compiling them again. Local only: GitHub's jobs keep Swatinem/
# rust-cache, and the compiler's output is the same either way. A pinned
# prebuilt release checked by its sha256.
ARG SCCACHE_VERSION=0.18.0
ARG SCCACHE_SHA256=45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89
RUN curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/sccache.tar.gz \
      "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
 && echo "${SCCACHE_SHA256}  /tmp/sccache.tar.gz" | sha256sum -c - \
 && tar -xzf /tmp/sccache.tar.gz -C /tmp \
 && install -m 0755 "/tmp/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache" /usr/local/bin/sccache \
 && rm -rf /tmp/sccache.tar.gz "/tmp/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl" \
 && mkdir -p /opt/sccache && chmod 0777 /opt/sccache \
 && sccache --version
