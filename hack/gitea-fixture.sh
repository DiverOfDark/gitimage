#!/usr/bin/env bash
# Throwaway Gitea (rootless image, built-in ssh server) for gitimage-git's ignored
# integration test. No root needed: podman (rootless) or docker.
#
#   hack/gitea-fixture.sh up <workdir>    # starts it, writes <workdir>/env
#   hack/gitea-fixture.sh down <workdir>  # removes the container
#
# Run the test with:
#   cargo test -p gitimage-git -- --ignored gitea
# (the test calls this script itself and skips if no container runtime is found).
#
# <workdir>/env (KEY=VALUE lines): HTTP, SSH_PORT, USER, PASSWORD, TOKEN, REPO, SHA,
# ROOT_TREE, CONTEXT, CONTEXT_TREE, KEYS (identity files), KNOWN_HOSTS, KNOWN_HOSTS_HASHED,
# CA (the https CA certificate). Needs git, openssl, ssh-keygen, ssh-keyscan, curl on the host.
set -euo pipefail

IMAGE="${GITEA_IMAGE:-docker.io/gitea/gitea:1.24-rootless}"
cmd="${1:?up|down}"
work="${2:?workdir}"
mkdir -p "$work"
work="$(cd "$work" && pwd)"

runtime="${CONTAINER_RUNTIME:-}"
if [[ -z "$runtime" ]]; then
  if command -v podman >/dev/null; then runtime=podman
  elif command -v docker >/dev/null; then runtime=docker
  else echo "no podman or docker" >&2; exit 2; fi
fi
name_file="$work/container"

if [[ "$cmd" == down ]]; then
  [[ -f "$name_file" ]] && "$runtime" rm -f "$(cat "$name_file")" >/dev/null 2>&1 || true
  exit 0
fi
[[ "$cmd" == up ]] || { echo "usage: $0 up|down <workdir>" >&2; exit 2; }

# TLS: gix never sends credentials over plain http. A throwaway CA + leaf for 127.0.0.1;
# clients trust it through SSL_CERT_FILE=<workdir>/ca.pem.
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 \
  -subj /CN=gitimage-test-ca -keyout "$work/ca.key" -out "$work/ca.pem" 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -subj /CN=127.0.0.1 -keyout "$work/tls.key" -out "$work/tls.csr" 2>/dev/null
printf 'subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > "$work/tls.ext"
openssl x509 -req -in "$work/tls.csr" -CA "$work/ca.pem" -CAkey "$work/ca.key" -CAcreateserial \
  -days 2 -extfile "$work/tls.ext" -out "$work/tls.pem" 2>/dev/null
chmod 644 "$work/tls.key"

name="gitimage-gitea-test-$$-$RANDOM"
echo "$name" > "$name_file"
"$runtime" create --rm --name "$name" \
  -p 127.0.0.1::3000 -p 127.0.0.1::2222 \
  -e GITEA__server__PROTOCOL=https \
  -e GITEA__server__CERT_FILE=/tmp/tls.pem \
  -e GITEA__server__KEY_FILE=/tmp/tls.key \
  -e GITEA__security__INSTALL_LOCK=true \
  -e GITEA__database__DB_TYPE=sqlite3 \
  -e GITEA__server__START_SSH_SERVER=true \
  -e GITEA__server__SSH_LISTEN_PORT=2222 \
  -e GITEA__service__DISABLE_REGISTRATION=true \
  -e GITEA__log__LEVEL=warn \
  "$IMAGE" >/dev/null
"$runtime" cp "$work/tls.pem" "$name:/tmp/tls.pem"
"$runtime" cp "$work/tls.key" "$name:/tmp/tls.key"
"$runtime" start "$name" >/dev/null

port_of() { "$runtime" port "$name" "$1" | head -n1 | sed 's/.*://'; }
http_port="$(port_of 3000/tcp)"
ssh_port="$(port_of 2222/tcp)"
http="https://127.0.0.1:$http_port"
export CURL_CA_BUNDLE="$work/ca.pem" GIT_SSL_CAINFO="$work/ca.pem"

for _ in $(seq 1 120); do
  curl -fsS "$http/api/v1/version" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "$http/api/v1/version" >/dev/null

user=gitimage
password="pw-$RANDOM-$RANDOM-Aa1"
"$runtime" exec "$name" gitea admin user create --admin --username "$user" \
  --password "$password" --email "$user@example.com" --must-change-password=false >/dev/null

api() { curl -fsS -u "$user:$password" -H 'Content-Type: application/json' "$@"; }
token="$(api -X POST "$http/api/v1/users/$user/tokens" \
  -d '{"name":"t","scopes":["write:repository","write:user"]}' | sed -n 's/.*"sha1":"\([0-9a-f]*\)".*/\1/p')"
[[ -n "$token" ]]
api -X POST "$http/api/v1/user/repos" -d '{"name":"app","private":true}' >/dev/null

keys=""
for t in ed25519 ecdsa rsa; do
  rm -f "$work/id_$t" "$work/id_$t.pub"
  ssh-keygen -q -t "$t" -N '' -C "gitimage-$t" -f "$work/id_$t"
  api -X POST "$http/api/v1/user/keys" \
    -d "{\"title\":\"$t\",\"key\":\"$(cat "$work/id_$t.pub")\"}" >/dev/null
  keys="$keys $work/id_$t"
done

# A commit with an exec bit, a symlink and a nested context.
src="$work/src"
rm -rf "$src"
mkdir -p "$src/services/api/bin" "$src/docs"
echo "hello" > "$src/README.md"
printf '#!/bin/sh\necho api\n' > "$src/services/api/bin/run.sh"
chmod +x "$src/services/api/bin/run.sh"
ln -s bin/run.sh "$src/services/api/run"
echo "FROM scratch" > "$src/services/api/Dockerfile"
echo "docs" > "$src/docs/index.md"
git -C "$src" init -q -b main
git -C "$src" -c user.name=t -c user.email=t@example.com add -A
git -C "$src" -c user.name=t -c user.email=t@example.com commit -q -m init
git -C "$src" push -q "https://$user:$password@127.0.0.1:$http_port/$user/app.git" main
sha="$(git -C "$src" rev-parse HEAD)"
# A second commit so the first is no longer the tip (still reachable).
echo "more" >> "$src/docs/index.md"
git -C "$src" -c user.name=t -c user.email=t@example.com commit -q -am second
git -C "$src" push -q "https://$user:$password@127.0.0.1:$http_port/$user/app.git" main

for _ in $(seq 1 60); do
  ssh-keyscan -p "$ssh_port" 127.0.0.1 2>/dev/null > "$work/known_hosts" || true
  [[ -s "$work/known_hosts" ]] && break
  sleep 0.5
done
[[ -s "$work/known_hosts" ]]
cp "$work/known_hosts" "$work/known_hosts_hashed"
ssh-keygen -q -H -f "$work/known_hosts_hashed" >/dev/null 2>&1
rm -f "$work/known_hosts_hashed.old"

cat > "$work/env" <<EOF
HTTP=$http
SSH_PORT=$ssh_port
USER=$user
PASSWORD=$password
TOKEN=$token
REPO=$user/app
SHA=$sha
TIP=$(git -C "$src" rev-parse HEAD)
ROOT_TREE=$(git -C "$src" rev-parse "$sha^{tree}")
CONTEXT=services/api
CONTEXT_TREE=$(git -C "$src" rev-parse "$sha:services/api")
KEYS=${keys# }
KNOWN_HOSTS=$work/known_hosts
KNOWN_HOSTS_HASHED=$work/known_hosts_hashed
CA=$work/ca.pem
EOF
echo "gitea up: $http ssh:$ssh_port ($name)"
