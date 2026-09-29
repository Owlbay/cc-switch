# TLS 测试证书（仅供测试）

`tests/tls_upstream.rs` 用这些文件起一个自签 HTTPS mock 上游，验证 `extra_ca_file`。
私钥只用于本地测试，不对应任何真实服务。

- `ca.pem`：测试 CA（P-256，有效期 100 年）。CA 私钥生成后已丢弃。
- `localhost.pem` / `localhost.key`：由测试 CA 签发的服务端证书，SAN 为 `DNS:localhost`、`IP:127.0.0.1`，私钥为 PKCS#8。

重新生成（需要同时替换三个文件）：

```bash
cat > /tmp/ca.cnf <<'EOF'
[req]
distinguished_name = dn
x509_extensions = v3_ca
prompt = no
[dn]
CN = cc-proxy-core test CA (test only)
[v3_ca]
basicConstraints = critical, CA:TRUE
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
EOF
cat > /tmp/leaf.cnf <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
CN = localhost
[v3_leaf]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost, IP:127.0.0.1
authorityKeyIdentifier = keyid
subjectKeyIdentifier = hash
EOF
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout /tmp/ca.key -out ca.pem -days 36500 -config /tmp/ca.cnf
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout localhost.key -out /tmp/leaf.csr -config /tmp/leaf.cnf
openssl x509 -req -in /tmp/leaf.csr -CA ca.pem -CAkey /tmp/ca.key -CAcreateserial -out localhost.pem -days 36500 -extfile /tmp/leaf.cnf -extensions v3_leaf
openssl pkcs8 -topk8 -nocrypt -in localhost.key -out localhost.key.tmp && mv localhost.key.tmp localhost.key
rm -f /tmp/ca.key /tmp/ca.srl /tmp/leaf.csr
```
