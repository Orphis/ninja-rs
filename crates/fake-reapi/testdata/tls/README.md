# Test certificates

For the fake server's TLS and mTLS tests only: the private keys are
public, so nothing must trust these certificates. Valid for 100 years,
EC P-256, keys in PKCS#8 (as Bazel's `--tls_client_key` expects).

- `ca.pem`: the test CA (its key was discarded).
- `server.pem`, `server-key.pem`: the server, for `localhost`, `127.0.0.1`
  and `rbe.test` (for `--tls_authority_override`).
- `client.pem`, `client-key.pem`: a client the test CA signed.
- `other-client.pem`, `other-client-key.pem`: a client another CA signed,
  which the server refuses.

Regenerate with OpenSSL: a CA (`basicConstraints=critical,CA:TRUE`,
`keyUsage=critical,keyCertSign,cRLSign`), and leaves signed by it with
`basicConstraints=critical,CA:FALSE`, `keyUsage=critical,digitalSignature`,
`extendedKeyUsage=serverAuth` (with the names above as `subjectAltName`)
or `clientAuth`.
