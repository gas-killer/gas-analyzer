# V2 leg of the setCode-drop measurement: the e2e's own `deploy_anvil.py
# --overlay` against a vanilla anvil. A container because the script needs
# pycryptodome (`Crypto.Hash.keccak`), which bench hosts need not have; anvil
# comes from the foundry release image so the node build is pinned.
FROM ghcr.io/foundry-rs/foundry:v1.5.1 AS foundry

FROM ubuntu:24.04
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends python3 python3-pip >/dev/null \
 && pip install --quiet --break-system-packages pycryptodome==3.23.0 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=foundry /usr/local/bin/anvil /usr/local/bin/anvil
COPY run-v2.sh /bench/run-v2.sh
ENTRYPOINT ["bash", "/bench/run-v2.sh"]
