@BIN@ @TAG@. Early software; see the README for what this version does and does not do.

## These binaries are not signed

The programs below are **not code-signed or notarized**. Your operating system will warn you the first
time you run one. That is expected for an unsigned program; it is also why you should check what you
downloaded before you run it (next section).

- **macOS:** Gatekeeper may say the program "cannot be opened" or "is from an unidentified developer"
  when the file was downloaded in a browser (the browser marks it as quarantined). Check it first, then
  remove the mark: `xattr -d com.apple.quarantine ./@BIN@`. Files fetched with `curl` or `gh release download` are not marked.
- **Windows:** SmartScreen may show "Windows protected your PC". After you have checked the file,
  choose "More info", then "Run anyway", or unblock the downloaded zip first with PowerShell:
  `Unblock-File .\@BIN@-@TAG@-x86_64-pc-windows-msvc.zip`.
- **Linux:** no warning. The build needs glibc 2.35 or newer (Ubuntu 22.04 or later, or similar).

## Check your download

`SHA256SUMS` lists the SHA-256 of every file in this release. Download it next to the archive.

```
# Linux
sha256sum -c --ignore-missing SHA256SUMS
# macOS
grep '<archive file name>' SHA256SUMS | shasum -a 256 -c -
# Windows (PowerShell): compare this with the line for your file in SHA256SUMS
(Get-FileHash .\<archive file name> -Algorithm SHA256).Hash
```

A checksum next to the file only shows the download is complete and matches this release page.
To also show the file was built from this repository's source by its release workflow, verify the
build provenance attestation with the GitHub CLI:

```
gh attestation verify <archive file name> --repo @REPO@
```

## Files

| Platform | File |
|---|---|
| Linux, x86-64 | `@BIN@-@TAG@-x86_64-unknown-linux-gnu.tar.gz` |
| macOS, Apple Silicon | `@BIN@-@TAG@-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `@BIN@-@TAG@-x86_64-apple-darwin.tar.gz` |
| Windows, x86-64 | `@BIN@-@TAG@-x86_64-pc-windows-msvc.zip` |

Each archive holds the program, `LICENSE` and `README.md`. Unpack it and run `@BIN@ --help`.
The Intel macOS build is cross-built on an Apple Silicon runner and is not run before release.
