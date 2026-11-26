Test-only CA and server certificate for tests/network_tests.rs (CN localhost, SAN localhost and 127.0.0.1,
valid 100 years). The private key is not a secret: it signs nothing outside these tests. The CA key was deleted
after signing. Regenerate with openssl (EC P-256): a self-signed CA with keyCertSign, and a server certificate with
extendedKeyUsage serverAuth; the server key in PKCS#8.
