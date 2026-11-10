#!/usr/bin/env bash
# Regenerates the loopback TLS test identity:
#   cert.pem       a test-only root CA, which test builds trust (TEST_ROOT_PEM);
#   localhost.p12  the `localhost` server certificate that root issued, with its
#                  key and the root as the chain (TEST_P12, password redjs-test).
#
# Apple's TLS accepts a server certificate only with a validity of 825 days or
# fewer and an extendedKeyUsage of serverAuth, even under a private root, so the
# server certificate must be re-issued before it expires (see its notAfter:
# `openssl pkcs12 -in localhost.p12 -nokeys -passin pass:redjs-test | openssl x509 -noout -enddate`).
# The root itself is not a server certificate and may live long.
set -euo pipefail
cd "$(dirname "$0")"
tmp=$(mktemp -d .gen-cert.XXXXXX)
trap 'rm -rf "$tmp"' EXIT

openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 36500 \
  -subj "/CN=redjs test root" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -keyout "$tmp/ca.key" -out cert.pem

openssl req -new -newkey rsa:2048 -nodes -sha256 \
  -subj "/CN=localhost" -keyout "$tmp/leaf.key" -out "$tmp/leaf.csr"
printf '%s\n' \
  "basicConstraints=critical,CA:FALSE" \
  "keyUsage=critical,digitalSignature,keyEncipherment" \
  "extendedKeyUsage=serverAuth" \
  "subjectAltName=DNS:localhost,IP:127.0.0.1" > "$tmp/leaf.ext"
openssl x509 -req -sha256 -days 825 -in "$tmp/leaf.csr" \
  -CA cert.pem -CAkey "$tmp/ca.key" -CAcreateserial -CAserial "$tmp/ca.srl" \
  -extfile "$tmp/leaf.ext" -out "$tmp/leaf.pem"

# SHA-1/3DES protection: the one every platform TLS here imports (macOS's
# Security framework, SChannel, OpenSSL 3 without the legacy provider).
openssl pkcs12 -export -name localhost -passout pass:redjs-test \
  -inkey "$tmp/leaf.key" -in "$tmp/leaf.pem" -certfile cert.pem \
  -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
  -out localhost.p12
