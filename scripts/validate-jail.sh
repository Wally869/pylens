#!/usr/bin/env bash
# pylens jail self-test — exercises the provisioned nsjail sandbox end to end:
#   correctness (mutation + aliasing), fork-server isolation, and the security boundaries
#   (no network, read-only rootfs). Run after provisioning:
#     bash scripts/validate-jail.sh        # native Linux
#     wsl -d <distro> -- bash /mnt/c/.../scripts/validate-jail.sh   # from Windows
set -uo pipefail

CFG="$HOME/.local/share/pylens/pylens.nsjail.cfg"
BIND="$HOME/.local/share/pylens:/pylens"
fails=0

jail() { nsjail --config "$CFG" --bindmount_ro "$BIND" -- /usr/bin/python3 /pylens/worker.py "$@" 2>/dev/null; }

check() { # name | expected-substring | actual
  if printf '%s' "$3" | grep -qF "$2"; then
    printf '  ok   %s\n' "$1"
  else
    printf '  FAIL %s\n     expected to contain: %s\n     got: %s\n' "$1" "$2" "$3"
    fails=$((fails + 1))
  fi
}

echo "[1] correctness: oneshot mutation + return-aliasing"
OUT="$(printf '%s' '{"source":"def f(xs):\n    xs.append(1)\n    return xs","fn":"f","args":[[0]]}' | jail)"
check "returns mutated arg [0,1]"   '"return": [0, 1]'        "$OUT"
check "return_aliases_arg = 0"      '"return_aliases_arg": 0' "$OUT"

echo "[2] fork-server isolation: a builtins monkeypatch must not leak to the next request"
OUT="$(printf '%s\n%s\n' \
  '{"source":"import builtins\ndef p(x):\n    builtins.len=lambda z:999\n    return len(x)","fn":"p","args":[[1,2,3]]}' \
  '{"source":"def q(x):\n    return len(x)","fn":"q","args":[[1,2,3]]}' | jail --serve)"
LAST="$(printf '%s' "$OUT" | tail -1)"
check "second call unaffected (len==3)" '"return": 3' "$LAST"

echo "[3] security: network is blocked (empty net namespace)"
OUT="$(printf '%s' '{"source":"import socket\ndef n(x):\n    s=socket.socket()\n    s.settimeout(3)\n    s.connect((\"1.1.1.1\",53))\n    return \"CONNECTED\"","fn":"n","args":[0]}' | jail)"
check "socket.connect raises (no CONNECTED)" '"ok": false' "$OUT"

echo "[4] security: rootfs is read-only (write outside /tmp fails)"
OUT="$(printf '%s' '{"source":"def w(x):\n    open(\"/pylens/evil\",\"w\").write(\"x\")\n    return \"WROTE\"","fn":"w","args":[0]}' | jail)"
check "write to /pylens raises (no WROTE)" '"ok": false' "$OUT"

echo "[5] sanity: /tmp IS writable"
OUT="$(printf '%s' '{"source":"def t(x):\n    open(\"/tmp/ok\",\"w\").write(\"x\")\n    return \"WROTE_TMP\"","fn":"t","args":[0]}' | jail)"
check "write to /tmp succeeds" 'WROTE_TMP' "$OUT"

echo
if [ "$fails" -eq 0 ]; then
  echo "ALL JAIL CHECKS PASSED"
else
  echo "$fails JAIL CHECK(S) FAILED"
  exit 1
fi
