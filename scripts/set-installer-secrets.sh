#!/usr/bin/env bash
# Set the GitHub secrets the release workflow needs to sign the macOS .pkg:
# MACOS_INSTALLER_CERT_P12_BASE64, MACOS_INSTALLER_CERT_PASSWORD and
# MACOS_INSTALLER_IDENTITY. Run it on a Mac with `gh` logged in.
#
# Two ways to use it:
#
#   No certificate yet: make a key and a signing request, have the Account
#   Holder create the certificate from it, then build the .p12 from that:
#     scripts/set-installer-secrets.sh csr
#     (upload ~/.loam-signing/installer.csr at developer.apple.com →
#      Certificates → + → Developer ID Installer, download the .cer)
#     scripts/set-installer-secrets.sh cer ~/Downloads/developerID_installer.cer
#
#   Already have a .p12 exported from Keychain Access:
#     scripts/set-installer-secrets.sh p12 ~/Desktop/installer.p12
#
# Set REPO to target another repository (default salishdev/loam).
set -euo pipefail

REPO=${REPO:-salishdev/loam}
DIR="$HOME/.loam-signing"
KEY="$DIR/installer.key"
CSR="$DIR/installer.csr"
# macOS ships LibreSSL here; its .p12 files import cleanly with `security`.
OPENSSL=/usr/bin/openssl
[ -x "$OPENSSL" ] || OPENSSL=openssl

die() { echo "error: $*" >&2; exit 1; }

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
  exit 1
}

need_gh() {
  command -v gh >/dev/null || die "gh is not installed"
  gh auth status >/dev/null 2>&1 || die "gh is not logged in; run: gh auth login"
}

# Print the certificate's common name, e.g.
# "Developer ID Installer: Astere Software LLC (WW59U84Z7X)".
common_name() {
  "$OPENSSL" x509 -noout -subject -nameopt multiline 2>/dev/null \
    | sed -n 's/^ *commonName *= *//p'
}

check_identity() {
  case "$1" in
    "Developer ID Installer: "*) ;;
    "") die "couldn't read the certificate's name" ;;
    *) die "this is a \"$1\" certificate, not a Developer ID Installer one" ;;
  esac
}

set_secrets() { # p12-file password identity
  local p12=$1 pass=$2 identity=$3
  echo "Setting secrets on $REPO for: $identity"
  base64 -i "$p12" | gh secret set MACOS_INSTALLER_CERT_P12_BASE64 -R "$REPO"
  printf '%s' "$pass" | gh secret set MACOS_INSTALLER_CERT_PASSWORD -R "$REPO"
  printf '%s' "$identity" | gh secret set MACOS_INSTALLER_IDENTITY -R "$REPO"
  echo "Done. The next release tag builds a signed, notarized .pkg."
}

cmd_csr() {
  mkdir -p "$DIR"
  chmod 700 "$DIR"
  [ -e "$KEY" ] && die "$KEY already exists; move it aside to start over"
  "$OPENSSL" genrsa -out "$KEY" 2048 2>/dev/null
  chmod 600 "$KEY"
  "$OPENSSL" req -new -key "$KEY" -out "$CSR" -subj "/CN=loam installer signing"
  cat <<EOF
Wrote the private key to $KEY (keep it; it never leaves this Mac)
and the signing request to $CSR.

Next, the Account Holder signs in at
https://developer.apple.com/account/resources/certificates/add
picks "Developer ID Installer", uploads $CSR and downloads the .cer. Then run:

  $0 cer <path to the .cer>
EOF
}

cmd_cer() {
  local cer=${1:-}
  [ -f "$cer" ] || die "give the path to the downloaded .cer"
  [ -f "$KEY" ] || die "no key at $KEY; the .cer must come from a CSR made with: $0 csr"
  need_gh
  local pem identity pass
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  pem="$TMP/cert.pem"
  # Apple hands out DER; accept PEM too.
  "$OPENSSL" x509 -inform DER -in "$cer" -out "$pem" 2>/dev/null \
    || "$OPENSSL" x509 -in "$cer" -out "$pem" 2>/dev/null \
    || die "$cer isn't a certificate"
  identity=$(common_name <"$pem")
  check_identity "$identity"
  [ "$("$OPENSSL" x509 -noout -modulus -in "$pem")" = "$("$OPENSSL" rsa -noout -modulus -in "$KEY" 2>/dev/null)" ] \
    || die "the certificate doesn't match the key in $KEY"
  pass=$(uuidgen)
  P12_PASS=$pass "$OPENSSL" pkcs12 -export -inkey "$KEY" -in "$pem" \
    -name "$identity" -out "$TMP/installer.p12" -passout env:P12_PASS
  set_secrets "$TMP/installer.p12" "$pass" "$identity"
}

cmd_p12() {
  local p12=${1:-}
  [ -f "$p12" ] || die "give the path to the exported .p12"
  need_gh
  local pass identity
  read -rsp "Password for $p12: " pass
  echo
  identity=$(P12_PASS=$pass "$OPENSSL" pkcs12 -in "$p12" -nokeys -clcerts \
      -passin env:P12_PASS 2>/dev/null | common_name) \
    || die "couldn't open $p12; wrong password?"
  check_identity "$identity"
  set_secrets "$p12" "$pass" "$identity"
}

case "${1:-}" in
  csr) cmd_csr ;;
  cer) shift; cmd_cer "$@" ;;
  p12) shift; cmd_p12 "$@" ;;
  *) usage ;;
esac
