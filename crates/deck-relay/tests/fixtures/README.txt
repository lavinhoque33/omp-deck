Test-only TLS material for tests/relay.rs (`in_process_tls`). Not secret, never use it anywhere
else. rcgen is not in Cargo.lock, so the pair is committed instead of generated in-test.

- ca.pem   self-signed test CA (its key was discarded after signing)
- cert.pem leaf for DNS:localhost and IP:127.0.0.1, signed by ca.pem, valid for 100 years
- key.pem  the leaf's P-256 key (PKCS#8)

Regenerate:
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 36500 \
    -subj "/CN=omp-deck-relay test CA" -keyout ca.key -out ca.pem \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign"
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=localhost" \
    -keyout key.pem -out leaf.csr
  printf 'basicConstraints=CA:FALSE\nsubjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nkeyUsage=critical,digitalSignature\n' > ext.cnf
  openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 36500 \
    -extfile ext.cnf -out cert.pem
  rm ca.key ca.srl leaf.csr ext.cnf
