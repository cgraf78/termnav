# shellcheck shell=bash

release_smoke_check() {
  local extract_root=$1 binary fake_bin ssh_log

  binary="$extract_root/$RELEASE_BINARY_DEST"
  "$binary" version >/dev/null
  "$binary" --help >/dev/null
  TERMNAV_REMOTE_LINK_HOST=release.example.invalid \
    "$binary" link-host | grep -Fxq release.example.invalid

  # The private SSH shim is packaged below share/, while the release binary is
  # now in bin/. Invoke the extracted layout exactly as Shdeps will expose it;
  # this prevents a source-tree-relative compatibility branch from masking a
  # recursive or missing production lookup.
  fake_bin=$extract_root/.smoke-bin
  ssh_log=$extract_root/.ssh-smoke.log
  mkdir "$fake_bin"
  cat >"$fake_bin/ssh" <<'EOF'
#!/usr/bin/env sh
printf '<%s>\n' "$@" >"$TERMNAV_RELEASE_SSH_LOG"
EOF
  chmod 0700 "$fake_bin/ssh"
  PATH="$extract_root/share/termnav/shims:$extract_root/bin:$fake_bin:/usr/bin:/bin" \
    TERMNAV_RELEASE_SSH_LOG="$ssh_log" \
    "$extract_root/share/termnav/shims/ssh" release.example.invalid true
  grep -Fxq '<release.example.invalid>' "$ssh_log"
  grep -Fxq '<true>' "$ssh_log"
}
