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
