# cert-contract-conformance

Bring the station's certificate handling into conformance with the perchpub
device-certificate contract (`docs/perchstation-csr-contract.md`): a unique,
self-consistent CSR subject (CN == DNS SAN), system-trust validation of the
`:443` enrollment edge, generate-once-and-reuse keypair, and a full-chain mTLS
upload identity — closing the audit findings F1–F7.
