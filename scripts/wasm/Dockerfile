# The wasm spike's image (docs/wasm-spike.md): the CI image
# (scripts/container/Dockerfile, tagged strand-ci:<hash> by
# scripts/container/run.sh) plus the wasm32-unknown-unknown target,
# wasm-bindgen-cli at the version of the wasm-bindgen crate in
# Cargo.lock, and binaryen's wasm-opt. Built and run by
# scripts/wasm/run.sh, which passes BASE.
ARG BASE
FROM ${BASE}

RUN rustup target add wasm32-unknown-unknown \
 && chmod -R a+rwX /opt/rustup

# wasm-bindgen's CLI must match the crate's version exactly.
ARG WASM_BINDGEN_VERSION=0.2.129
RUN cargo install --locked --root /opt/wasm-tools wasm-bindgen-cli --version ${WASM_BINDGEN_VERSION} \
 && rm -rf /opt/cargo/registry/* /opt/cargo/git/*

# binaryen's wasm-opt, a pinned prebuilt release checked by its sha256.
ARG BINARYEN_VERSION=124
ARG BINARYEN_SHA256=0290c3779fedf592b8da0ded3032ff55c41a2b7bfa2d6bf7b7bac6f0e6e28963
RUN curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/binaryen.tar.gz \
      "https://github.com/WebAssembly/binaryen/releases/download/version_${BINARYEN_VERSION}/binaryen-version_${BINARYEN_VERSION}-x86_64-linux.tar.gz" \
 && echo "${BINARYEN_SHA256}  /tmp/binaryen.tar.gz" | sha256sum -c - \
 && tar -xzf /tmp/binaryen.tar.gz -C /opt \
 && ln -s /opt/binaryen-version_${BINARYEN_VERSION}/bin/wasm-opt /usr/local/bin/wasm-opt \
 && rm /tmp/binaryen.tar.gz \
 && wasm-opt --version

RUN apt-get update && apt-get install -y --no-install-recommends brotli && rm -rf /var/lib/apt/lists/*

ENV PATH=/opt/wasm-tools/bin:$PATH
RUN wasm-bindgen --version
